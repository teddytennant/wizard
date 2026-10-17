// OWNER: bottom-pane
//! The composer's editable buffer, ported from Codex's `bottom_pane/textarea.rs` (spec C.1.5,
//! C.2.6): raw UTF-8 text, atomic elements (pasted-content placeholders, completed slash
//! commands), a single-entry kill buffer, and wrapping that always leaves the insertion point
//! a visible cell.
//!
//! Wrapping runs on `crate::wrap` instead of `textwrap`. Rows are byte ranges that carry one
//! sentinel byte past their end (the newline, or the end of the text), as in Codex, so the
//! cursor maps to a row by `start <= pos`.

use std::borrow::Cow;
use std::cell::{Ref, RefCell};
use std::ops::Range;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use unicode_segmentation::UnicodeSegmentation;

use super::key_hint::{KeyBinding, KeyBindingListExt, alt, ctrl, ctrl_alt, plain, shift};
use crate::wrap::{width_of, wrap_ranges_trim};

pub mod vim;

const WORD_SEPARATORS: &str = "`~!@#$%^&*()-=+[{]}\\|;:'\",.<>/?";

fn is_word_separator(ch: char) -> bool {
    WORD_SEPARATORS.contains(ch)
}

/// Split a whitespace-free run into pieces that are all separators or all word characters.
fn split_word_pieces(run: &str) -> Vec<(usize, &str)> {
    let mut pieces = Vec::new();
    for (segment_start, segment) in run.split_word_bound_indices() {
        let mut piece_start = 0;
        let mut chars = segment.char_indices();
        let Some((_, first_char)) = chars.next() else {
            continue;
        };
        let mut in_separator = is_word_separator(first_char);
        for (idx, ch) in chars {
            let is_separator = is_word_separator(ch);
            if is_separator == in_separator {
                continue;
            }
            pieces.push((segment_start + piece_start, &segment[piece_start..idx]));
            piece_start = idx;
            in_separator = is_separator;
        }
        pieces.push((segment_start + piece_start, &segment[piece_start..]));
    }
    pieces
}

/// A tab renders and wraps as one column; the byte length stays the same so ranges still index
/// the editable text.
fn text_for_display(text: &str) -> Cow<'_, str> {
    if text.contains('\t') {
        Cow::Owned(text.replace('\t', " "))
    } else {
        Cow::Borrowed(text)
    }
}

#[derive(Debug, Clone)]
struct TextElement {
    range: Range<usize>,
}

#[derive(Debug, Clone)]
struct WrapCache {
    width: u16,
    lines: Vec<Range<usize>>,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct TextAreaState {
    /// Index of the first visible wrapped row.
    pub scroll: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KillBufferKind {
    Characterwise,
    Linewise,
}

/// Default editor bindings (spec C.2.6).
struct Bindings {
    insert_newline: Vec<KeyBinding>,
    move_left: Vec<KeyBinding>,
    move_right: Vec<KeyBinding>,
    move_up: Vec<KeyBinding>,
    move_down: Vec<KeyBinding>,
    move_word_left: Vec<KeyBinding>,
    move_word_right: Vec<KeyBinding>,
    move_line_start: Vec<KeyBinding>,
    move_line_end: Vec<KeyBinding>,
    delete_backward: Vec<KeyBinding>,
    delete_forward: Vec<KeyBinding>,
    delete_backward_word: Vec<KeyBinding>,
    delete_forward_word: Vec<KeyBinding>,
    kill_line_start: Vec<KeyBinding>,
    kill_line_end: Vec<KeyBinding>,
    yank: Vec<KeyBinding>,
}

impl Bindings {
    fn defaults() -> Self {
        use KeyCode::*;
        Self {
            insert_newline: vec![
                ctrl(Char('j')),
                ctrl(Char('m')),
                plain(Enter),
                shift(Enter),
                alt(Enter),
            ],
            move_left: vec![plain(Left), ctrl(Char('b'))],
            move_right: vec![plain(Right), ctrl(Char('f'))],
            move_up: vec![plain(Up), ctrl(Char('p'))],
            move_down: vec![plain(Down), ctrl(Char('n'))],
            move_word_left: vec![alt(Char('b')), alt(Left), ctrl(Left)],
            move_word_right: vec![alt(Char('f')), alt(Right), ctrl(Right)],
            move_line_start: vec![plain(Home), ctrl(Char('a'))],
            move_line_end: vec![plain(End), ctrl(Char('e'))],
            delete_backward: vec![plain(Backspace), shift(Backspace), ctrl(Char('h'))],
            delete_forward: vec![plain(Delete), shift(Delete), ctrl(Char('d'))],
            delete_backward_word: vec![
                alt(Backspace),
                ctrl(Backspace),
                KeyBinding::new(Backspace, KeyModifiers::CONTROL | KeyModifiers::SHIFT),
                ctrl(Char('w')),
                ctrl_alt(Char('h')),
            ],
            delete_forward_word: vec![
                alt(Delete),
                ctrl(Delete),
                KeyBinding::new(Delete, KeyModifiers::CONTROL | KeyModifiers::SHIFT),
                alt(Char('d')),
            ],
            kill_line_start: vec![ctrl(Char('u'))],
            kill_line_end: vec![ctrl(Char('k'))],
            yank: vec![ctrl(Char('y'))],
        }
    }
}

thread_local! {
    static BINDINGS: Bindings = Bindings::defaults();
}

#[derive(Debug)]
pub struct TextArea {
    text: String,
    cursor_pos: usize,
    wrap_cache: RefCell<Option<WrapCache>>,
    preferred_col: Option<usize>,
    elements: Vec<TextElement>,
    kill_buffer: String,
    kill_buffer_kind: KillBufferKind,
    pub(crate) vim: vim::VimState,
}

impl Default for TextArea {
    fn default() -> Self {
        Self::new()
    }
}

impl TextArea {
    pub fn new() -> Self {
        Self {
            text: String::new(),
            cursor_pos: 0,
            wrap_cache: RefCell::new(None),
            preferred_col: None,
            elements: Vec::new(),
            kill_buffer: String::new(),
            kill_buffer_kind: KillBufferKind::Characterwise,
            vim: vim::VimState::default(),
        }
    }

    // ---- whole-buffer replacement ---------------------------------------------------------

    /// Replace the text and drop every element. The kill buffer survives so Ctrl+Y still
    /// restores the last Ctrl+K after a submit or a synthetic clear.
    pub fn set_text_clearing_elements(&mut self, text: &str) {
        self.set_text_inner(text, None);
    }

    pub fn set_text_with_elements(&mut self, text: &str, elements: &[Range<usize>]) {
        self.set_text_inner(text, Some(elements));
    }

    fn set_text_inner(&mut self, text: &str, elements: Option<&[Range<usize>]>) {
        self.text = text.to_string();
        self.cursor_pos = self.cursor_pos.clamp(0, self.text.len());
        self.elements.clear();
        if let Some(elements) = elements {
            for r in elements {
                let start = self.clamp_pos_to_char_boundary(r.start.min(self.text.len()));
                let end = self.clamp_pos_to_char_boundary(r.end.min(self.text.len()));
                if start >= end {
                    continue;
                }
                self.elements.push(TextElement { range: start..end });
            }
            self.elements.sort_by_key(|e| e.range.start);
        }
        self.cursor_pos = self.clamp_pos_to_nearest_boundary(self.cursor_pos);
        self.wrap_cache.replace(None);
        self.preferred_col = None;
    }

    // ---- reads ----------------------------------------------------------------------------

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn cursor(&self) -> usize {
        self.cursor_pos
    }

    pub fn desired_height(&self, width: u16) -> u16 {
        self.wrapped_lines(width).len() as u16
    }

    pub fn kill_buffer(&self) -> &str {
        &self.kill_buffer
    }

    // ---- editing --------------------------------------------------------------------------

    /// Insert pasted text at the cursor (no placeholder logic; the composer does that).
    pub fn paste(&mut self, text: &str) {
        self.insert_str(&text.replace("\r\n", "\n").replace('\r', "\n"));
    }

    pub fn insert_str(&mut self, text: &str) {
        self.insert_str_at(self.cursor_pos, text);
    }

    pub fn insert_str_at(&mut self, pos: usize, text: &str) {
        let pos = self.clamp_pos_for_insertion(pos);
        self.text.insert_str(pos, text);
        self.wrap_cache.replace(None);
        if pos <= self.cursor_pos {
            self.cursor_pos += text.len();
        }
        self.shift_elements(pos, 0, text.len());
        self.preferred_col = None;
    }

    pub fn replace_range(&mut self, range: Range<usize>, text: &str) {
        let range = self.expand_range_to_element_boundaries(range);
        self.replace_range_raw(range, text);
    }

    fn replace_range_raw(&mut self, range: Range<usize>, text: &str) {
        assert!(range.start <= range.end);
        let start = range.start.min(self.text.len());
        let end = range.end.min(self.text.len());
        let removed_len = end - start;
        let inserted_len = text.len();
        if removed_len == 0 && inserted_len == 0 {
            return;
        }
        let diff = inserted_len as isize - removed_len as isize;
        self.text.replace_range(start..end, text);
        self.wrap_cache.replace(None);
        self.preferred_col = None;
        self.shift_elements(start, end - start, inserted_len);
        self.cursor_pos = if self.cursor_pos < start {
            self.cursor_pos
        } else if self.cursor_pos <= end {
            start + inserted_len
        } else {
            ((self.cursor_pos as isize) + diff) as usize
        }
        .min(self.text.len());
        self.cursor_pos = self.clamp_pos_to_nearest_boundary(self.cursor_pos);
    }

    pub fn set_cursor(&mut self, pos: usize) {
        self.cursor_pos = pos.min(self.text.len());
        self.cursor_pos = self.clamp_pos_to_nearest_boundary(self.cursor_pos);
        self.preferred_col = None;
    }

    pub fn delete_backward(&mut self, n: usize) {
        if n == 0 || self.cursor_pos == 0 {
            return;
        }
        let mut target = self.cursor_pos;
        for _ in 0..n {
            target = self.prev_atomic_boundary(target);
            if target == 0 {
                break;
            }
        }
        self.replace_range(target..self.cursor_pos, "");
    }

    pub fn delete_forward(&mut self, n: usize) {
        if n == 0 || self.cursor_pos >= self.text.len() {
            return;
        }
        let mut target = self.cursor_pos;
        for _ in 0..n {
            target = self.next_atomic_boundary(target);
            if target >= self.text.len() {
                break;
            }
        }
        self.replace_range(self.cursor_pos..target, "");
    }

    pub fn delete_backward_word(&mut self) {
        let start = self.beginning_of_previous_word();
        self.kill_range(start..self.cursor_pos);
    }

    pub fn delete_forward_word(&mut self) {
        let end = self.end_of_next_word();
        if end > self.cursor_pos {
            self.kill_range(self.cursor_pos..end);
        }
    }

    /// Kill to the end of the logical line; at the end of a line the newline goes, so repeated
    /// presses keep making progress.
    pub fn kill_to_end_of_line(&mut self) {
        let eol = self.end_of_current_line();
        let range = if self.cursor_pos == eol {
            (eol < self.text.len()).then(|| self.cursor_pos..eol + 1)
        } else {
            Some(self.cursor_pos..eol)
        };
        if let Some(r) = range {
            self.kill_range(r);
        }
    }

    pub fn kill_to_beginning_of_line(&mut self) {
        let bol = self.beginning_of_current_line();
        let range = if self.cursor_pos == bol {
            (bol > 0).then(|| bol - 1..bol)
        } else {
            Some(bol..self.cursor_pos)
        };
        if let Some(r) = range {
            self.kill_range(r);
        }
    }

    pub fn yank(&mut self) {
        if self.kill_buffer.is_empty() {
            return;
        }
        let text = self.kill_buffer.clone();
        self.insert_str(&text);
    }

    pub(crate) fn kill_range(&mut self, range: Range<usize>) {
        self.kill_range_with_kind(range, KillBufferKind::Characterwise);
    }

    pub(crate) fn kill_range_with_kind(&mut self, range: Range<usize>, kind: KillBufferKind) {
        let range = self.expand_range_to_element_boundaries(range);
        if range.start >= range.end {
            return;
        }
        let removed = self.text[range.clone()].to_string();
        if removed.is_empty() {
            return;
        }
        self.kill_buffer = removed;
        self.kill_buffer_kind = kind;
        self.replace_range_raw(range, "");
    }

    pub(crate) fn yank_range_with_kind(&mut self, range: Range<usize>, kind: KillBufferKind) {
        let range = self.expand_range_to_element_boundaries(range);
        if range.start >= range.end {
            return;
        }
        let text = self.text[range].to_string();
        if text.is_empty() {
            return;
        }
        self.kill_buffer = text;
        self.kill_buffer_kind = kind;
    }

    // ---- cursor motion --------------------------------------------------------------------

    pub fn move_cursor_left(&mut self) {
        self.cursor_pos = self.prev_atomic_boundary(self.cursor_pos);
        self.preferred_col = None;
    }

    pub fn move_cursor_right(&mut self) {
        self.cursor_pos = self.next_atomic_boundary(self.cursor_pos);
        self.preferred_col = None;
    }

    /// Move one wrapped row up, keeping the column; on the first row jump to the start.
    pub fn move_cursor_up(&mut self, width: u16) {
        let lines = self.wrapped_lines(width).clone();
        let Some(idx) = Self::wrapped_line_index_by_start(&lines, self.cursor_pos) else {
            return;
        };
        let cur = &lines[idx];
        let target_col = self
            .preferred_col
            .unwrap_or_else(|| width_of(&self.text[cur.start..self.cursor_pos]));
        if idx == 0 {
            self.cursor_pos = 0;
            self.preferred_col = None;
            return;
        }
        let prev = &lines[idx - 1];
        let line_start = prev.start;
        let mut line_end = prev.end.saturating_sub(1);
        if line_end == cur.start {
            line_end = self.prev_atomic_boundary(line_end).max(line_start);
        }
        if self.preferred_col.is_none() {
            self.preferred_col = Some(target_col);
        }
        self.move_to_display_col_on_line(line_start, line_end, target_col);
    }

    pub fn move_cursor_down(&mut self, width: u16) {
        let lines = self.wrapped_lines(width).clone();
        let Some(idx) = Self::wrapped_line_index_by_start(&lines, self.cursor_pos) else {
            return;
        };
        let cur = &lines[idx];
        let target_col = self
            .preferred_col
            .unwrap_or_else(|| width_of(&self.text[cur.start..self.cursor_pos]));
        if idx + 1 >= lines.len() {
            self.cursor_pos = self.text.len();
            self.preferred_col = None;
            return;
        }
        let next = &lines[idx + 1];
        let line_start = next.start;
        let mut line_end = next.end.saturating_sub(1);
        if lines
            .get(idx + 2)
            .is_some_and(|following| following.start == line_end)
        {
            line_end = self.prev_atomic_boundary(line_end).max(line_start);
        }
        if self.preferred_col.is_none() {
            self.preferred_col = Some(target_col);
        }
        self.move_to_display_col_on_line(line_start, line_end, target_col);
    }

    pub fn move_cursor_to_beginning_of_line(&mut self, move_up_at_bol: bool) {
        let bol = self.beginning_of_current_line();
        if move_up_at_bol && self.cursor_pos == bol {
            self.set_cursor(self.beginning_of_line(self.cursor_pos.saturating_sub(1)));
        } else {
            self.set_cursor(bol);
        }
        self.preferred_col = None;
    }

    pub fn move_cursor_to_end_of_line(&mut self, move_down_at_eol: bool) {
        let eol = self.end_of_current_line();
        if move_down_at_eol && self.cursor_pos == eol {
            let next_pos = (self.cursor_pos + 1).min(self.text.len());
            self.set_cursor(self.end_of_line(next_pos));
        } else {
            self.set_cursor(eol);
        }
    }

    /// True when Up would leave the text: the cursor is on the first wrapped row.
    pub fn cursor_on_first_row(&self, width: u16) -> bool {
        let lines = self.wrapped_lines(width);
        Self::wrapped_line_index_by_start(&lines, self.cursor_pos).is_none_or(|i| i == 0)
    }

    pub fn cursor_on_last_row(&self, width: u16) -> bool {
        let lines = self.wrapped_lines(width);
        Self::wrapped_line_index_by_start(&lines, self.cursor_pos)
            .is_none_or(|i| i + 1 >= lines.len())
    }

    fn move_to_display_col_on_line(&mut self, line_start: usize, line_end: usize, target: usize) {
        let mut w = 0usize;
        for (i, g) in self.text[line_start..line_end].grapheme_indices(true) {
            w += width_of(g);
            if w > target {
                self.cursor_pos = self.clamp_pos_to_nearest_boundary(line_start + i);
                return;
            }
        }
        self.cursor_pos = self.clamp_pos_to_nearest_boundary(line_end);
    }

    pub(crate) fn beginning_of_line(&self, pos: usize) -> usize {
        self.text[..pos].rfind('\n').map(|i| i + 1).unwrap_or(0)
    }

    pub(crate) fn beginning_of_current_line(&self) -> usize {
        self.beginning_of_line(self.cursor_pos)
    }

    pub(crate) fn first_non_blank_of_current_line(&self) -> usize {
        let bol = self.beginning_of_current_line();
        let eol = self.end_of_current_line();
        self.text[bol..eol]
            .char_indices()
            .find_map(|(o, ch)| (!ch.is_whitespace()).then_some(bol + o))
            .unwrap_or(eol)
    }

    pub(crate) fn end_of_line(&self, pos: usize) -> usize {
        self.text[pos..]
            .find('\n')
            .map(|i| i + pos)
            .unwrap_or(self.text.len())
    }

    pub(crate) fn end_of_current_line(&self) -> usize {
        self.end_of_line(self.cursor_pos)
    }

    // ---- key input ------------------------------------------------------------------------

    /// Apply the default editor bindings (and Vim, when on). `width` is the textarea width, for
    /// vertical motion over wrapped rows.
    pub fn input(&mut self, event: KeyEvent, width: u16) {
        if !matches!(event.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return;
        }
        if self.vim.enabled {
            self.handle_vim_input(event, width);
            return;
        }
        self.input_insert_mode(event, width);
    }

    pub(crate) fn input_insert_mode(&mut self, event: KeyEvent, width: u16) {
        BINDINGS.with(|b| {
            if b.insert_newline.is_pressed(event) {
                self.insert_str("\n");
            } else if b.delete_backward_word.is_pressed(event) {
                self.delete_backward_word();
            } else if b.delete_backward.is_pressed(event) {
                self.delete_backward(1);
            } else if b.delete_forward_word.is_pressed(event) {
                self.delete_forward_word();
            } else if b.delete_forward.is_pressed(event) {
                self.delete_forward(1);
            } else if b.kill_line_start.is_pressed(event) {
                self.kill_to_beginning_of_line();
            } else if b.kill_line_end.is_pressed(event) {
                self.kill_to_end_of_line();
            } else if b.yank.is_pressed(event) {
                self.yank();
            } else if b.move_word_left.is_pressed(event) {
                self.set_cursor(self.beginning_of_previous_word());
            } else if b.move_word_right.is_pressed(event) {
                self.set_cursor(self.end_of_next_word());
            } else if b.move_left.is_pressed(event) {
                self.move_cursor_left();
            } else if b.move_right.is_pressed(event) {
                self.move_cursor_right();
            } else if b.move_up.is_pressed(event) {
                self.move_cursor_up(width);
            } else if b.move_down.is_pressed(event) {
                self.move_cursor_down(width);
            } else if b.move_line_start.is_pressed(event) {
                let up = matches!(
                    event,
                    KeyEvent {
                        code: KeyCode::Char('a'),
                        modifiers: KeyModifiers::CONTROL,
                        ..
                    }
                );
                self.move_cursor_to_beginning_of_line(up);
            } else if b.move_line_end.is_pressed(event) {
                let down = matches!(
                    event,
                    KeyEvent {
                        code: KeyCode::Char('e'),
                        modifiers: KeyModifiers::CONTROL,
                        ..
                    }
                );
                self.move_cursor_to_end_of_line(down);
            } else if let KeyEvent {
                code: KeyCode::Char(c),
                modifiers: KeyModifiers::NONE | KeyModifiers::SHIFT,
                ..
            } = event
            {
                // Alt-modified characters are not text: terminals send them for Meta chords.
                if !c.is_ascii_control() {
                    self.insert_str(&c.to_string());
                }
            } else if let KeyEvent {
                code: KeyCode::Tab,
                modifiers: KeyModifiers::NONE,
                ..
            } = event
            {
                self.insert_str("\t");
            }
        });
    }

    // ---- elements -------------------------------------------------------------------------

    pub fn element_payloads(&self) -> Vec<String> {
        self.elements
            .iter()
            .filter_map(|e| self.text.get(e.range.clone()).map(str::to_string))
            .collect()
    }

    pub fn element_ranges(&self) -> Vec<Range<usize>> {
        self.elements.iter().map(|e| e.range.clone()).collect()
    }

    /// Insert `text` at the cursor as one atomic element; the cursor ends after it.
    pub fn insert_element(&mut self, text: &str) {
        let start = self.clamp_pos_for_insertion(self.cursor_pos);
        self.insert_str_at(start, text);
        let end = start + text.len();
        self.add_element(start..end);
        self.set_cursor(end);
    }

    fn add_element(&mut self, range: Range<usize>) {
        self.elements.push(TextElement { range });
        self.elements.sort_by_key(|e| e.range.start);
    }

    /// Mark typed text as an element without changing it (a completed `/review`).
    pub fn add_element_range(&mut self, range: Range<usize>) -> bool {
        let start = self.clamp_pos_to_char_boundary(range.start.min(self.text.len()));
        let end = self.clamp_pos_to_char_boundary(range.end.min(self.text.len()));
        if start >= end {
            return false;
        }
        if self
            .elements
            .iter()
            .any(|e| start < e.range.end && end > e.range.start)
        {
            return false;
        }
        self.add_element(start..end);
        true
    }

    pub fn remove_element_range(&mut self, range: Range<usize>) -> bool {
        let before = self.elements.len();
        self.elements
            .retain(|e| e.range.start != range.start || e.range.end != range.end);
        before != self.elements.len()
    }

    /// Rewrite the text of every element whose text is `old` to `new`.
    pub fn replace_element_payload(&mut self, old: &str, new: &str) -> bool {
        let Some(range) = self
            .elements
            .iter()
            .find(|e| self.text.get(e.range.clone()) == Some(old))
            .map(|e| e.range.clone())
        else {
            return false;
        };
        let cursor = self.cursor_pos;
        self.replace_range_raw(range.clone(), new);
        // The element survives as one element covering the new text.
        self.elements.retain(|e| e.range.start != range.start);
        self.add_element(range.start..range.start + new.len());
        let delta = new.len() as isize - old.len() as isize;
        let _ = cursor;
        let _ = delta;
        true
    }

    fn find_element_containing(&self, pos: usize) -> Option<usize> {
        self.elements
            .iter()
            .position(|e| pos > e.range.start && pos < e.range.end)
    }

    fn clamp_pos_to_char_boundary(&self, pos: usize) -> usize {
        let pos = pos.min(self.text.len());
        if self.text.is_char_boundary(pos) {
            return pos;
        }
        let mut prev = pos;
        while prev > 0 && !self.text.is_char_boundary(prev) {
            prev -= 1;
        }
        let mut next = pos;
        while next < self.text.len() && !self.text.is_char_boundary(next) {
            next += 1;
        }
        if pos - prev <= next - pos { prev } else { next }
    }

    fn clamp_pos_to_nearest_boundary(&self, pos: usize) -> usize {
        let pos = self.clamp_pos_to_char_boundary(pos);
        match self.find_element_containing(pos) {
            Some(i) => {
                let e = &self.elements[i];
                if pos - e.range.start <= e.range.end - pos {
                    self.clamp_pos_to_char_boundary(e.range.start)
                } else {
                    self.clamp_pos_to_char_boundary(e.range.end)
                }
            }
            None => pos,
        }
    }

    fn clamp_pos_for_insertion(&self, pos: usize) -> usize {
        self.clamp_pos_to_nearest_boundary(pos)
    }

    fn expand_range_to_element_boundaries(&self, mut range: Range<usize>) -> Range<usize> {
        loop {
            let mut changed = false;
            for e in &self.elements {
                if e.range.start < range.end && e.range.end > range.start {
                    let s = range.start.min(e.range.start);
                    let en = range.end.max(e.range.end);
                    if s != range.start || en != range.end {
                        range = s..en;
                        changed = true;
                    }
                }
            }
            if !changed {
                break;
            }
        }
        range
    }

    fn shift_elements(&mut self, at: usize, removed: usize, inserted: usize) {
        let end = at + removed;
        let diff = inserted as isize - removed as isize;
        self.elements
            .retain(|e| !(e.range.start >= at && e.range.end <= end && removed > 0));
        for e in &mut self.elements {
            if e.range.end <= at {
                // before the edit
            } else if e.range.start >= end {
                e.range.start = ((e.range.start as isize) + diff) as usize;
                e.range.end = ((e.range.end as isize) + diff) as usize;
            } else {
                let ns = at.min(e.range.start);
                let ne = at + inserted.max(e.range.end.saturating_sub(end));
                e.range = ns..ne;
            }
        }
    }

    pub(crate) fn prev_atomic_boundary(&self, pos: usize) -> usize {
        if pos == 0 {
            return 0;
        }
        if let Some(i) = self
            .elements
            .iter()
            .position(|e| pos > e.range.start && pos <= e.range.end)
        {
            return self.elements[i].range.start;
        }
        let mut gc = unicode_segmentation::GraphemeCursor::new(pos, self.text.len(), false);
        match gc.prev_boundary(&self.text, 0) {
            Ok(Some(b)) => match self.find_element_containing(b) {
                Some(i) => self.elements[i].range.start,
                None => b,
            },
            Ok(None) => 0,
            Err(_) => pos.saturating_sub(1),
        }
    }

    pub(crate) fn next_atomic_boundary(&self, pos: usize) -> usize {
        if pos >= self.text.len() {
            return self.text.len();
        }
        if let Some(i) = self
            .elements
            .iter()
            .position(|e| pos >= e.range.start && pos < e.range.end)
        {
            return self.elements[i].range.end;
        }
        let mut gc = unicode_segmentation::GraphemeCursor::new(pos, self.text.len(), false);
        match gc.next_boundary(&self.text, 0) {
            Ok(Some(b)) => match self.find_element_containing(b) {
                Some(i) => self.elements[i].range.end,
                None => b,
            },
            Ok(None) => self.text.len(),
            Err(_) => pos.saturating_add(1),
        }
    }

    // ---- words ----------------------------------------------------------------------------

    pub(crate) fn beginning_of_previous_word(&self) -> usize {
        let prefix = &self.text[..self.cursor_pos];
        let Some((first_non_ws_idx, ch)) = prefix
            .char_indices()
            .rev()
            .find(|&(_, ch)| !ch.is_whitespace())
        else {
            return 0;
        };
        let run_start = prefix[..first_non_ws_idx]
            .char_indices()
            .rev()
            .find(|&(_, ch)| ch.is_whitespace())
            .map_or(0, |(idx, ch)| idx + ch.len_utf8());
        let run_end = first_non_ws_idx + ch.len_utf8();
        let pieces = split_word_pieces(&prefix[run_start..run_end]);
        let mut pieces = pieces.into_iter().rev().peekable();
        let Some((piece_start, piece)) = pieces.next() else {
            return run_start;
        };
        let mut start = run_start + piece_start;
        if piece.chars().all(is_word_separator) {
            while let Some((idx, piece)) = pieces.peek() {
                if !piece.chars().all(is_word_separator) {
                    break;
                }
                start = run_start + *idx;
                pieces.next();
            }
        }
        self.adjust_pos_out_of_elements(start, true)
    }

    pub(crate) fn end_of_next_word(&self) -> usize {
        self.end_of_next_word_from(self.cursor_pos)
    }

    pub(crate) fn end_of_next_word_from(&self, cursor_pos: usize) -> usize {
        let suffix = &self.text[cursor_pos..];
        let Some(first_non_ws) = suffix.find(|ch: char| !ch.is_whitespace()) else {
            return self.text.len();
        };
        let run = &suffix[first_non_ws..];
        let run = &run[..run.find(char::is_whitespace).unwrap_or(run.len())];
        let mut pieces = split_word_pieces(run).into_iter().peekable();
        let Some((start, piece)) = pieces.next() else {
            return cursor_pos + first_non_ws;
        };
        let word_start = cursor_pos + first_non_ws + start;
        let mut end = word_start + piece.len();
        if piece.chars().all(is_word_separator) {
            while let Some((idx, piece)) = pieces.peek() {
                if !piece.chars().all(is_word_separator) {
                    break;
                }
                end = cursor_pos + first_non_ws + *idx + piece.len();
                pieces.next();
            }
        }
        self.adjust_pos_out_of_elements(end, false)
    }

    pub(crate) fn beginning_of_next_word(&self) -> usize {
        let Some(first_non_ws) = self.text[self.cursor_pos..].find(|c: char| !c.is_whitespace())
        else {
            return self.text.len();
        };
        let word_start = self.cursor_pos + first_non_ws;
        if word_start != self.cursor_pos {
            return self.adjust_pos_out_of_elements(word_start, true);
        }
        let end = self.end_of_next_word();
        if end >= self.text.len() {
            return self.text.len();
        }
        let Some(next_non_ws) = self.text[end..].find(|c: char| !c.is_whitespace()) else {
            return self.text.len();
        };
        self.adjust_pos_out_of_elements(end + next_non_ws, true)
    }

    fn adjust_pos_out_of_elements(&self, pos: usize, prefer_start: bool) -> usize {
        match self.find_element_containing(pos) {
            Some(i) if prefer_start => self.elements[i].range.start,
            Some(i) => self.elements[i].range.end,
            None => pos,
        }
    }

    // ---- wrapping and drawing -------------------------------------------------------------

    fn wrapped_line_index_by_start(lines: &[Range<usize>], pos: usize) -> Option<usize> {
        let idx = lines.partition_point(|r| r.start <= pos);
        if idx == 0 { None } else { Some(idx - 1) }
    }

    /// Rows as byte ranges with a sentinel byte past each end. Overflowing spaces get their own
    /// rows and a full logical line gets one more row so the insertion point stays visible.
    pub fn wrapped_lines(&self, width: u16) -> Ref<'_, Vec<Range<usize>>> {
        {
            let mut cache = self.wrap_cache.borrow_mut();
            if cache.as_ref().is_none_or(|c| c.width != width) {
                let display = text_for_display(&self.text);
                let lines = wrap_rows(display.as_ref(), width);
                *cache = Some(WrapCache { width, lines });
            }
        }
        Ref::map(self.wrap_cache.borrow(), |c| &c.as_ref().unwrap().lines)
    }

    /// Scroll that keeps the cursor row on screen and shows everything when it all fits.
    fn effective_scroll(&self, area_height: u16, lines: &[Range<usize>], current: u16) -> u16 {
        let total = lines.len() as u16;
        if area_height >= total {
            return 0;
        }
        let cursor_line =
            Self::wrapped_line_index_by_start(lines, self.cursor_pos).unwrap_or(0) as u16;
        let max_scroll = total.saturating_sub(area_height);
        let mut scroll = current.min(max_scroll);
        if cursor_line < scroll {
            scroll = cursor_line;
        } else if cursor_line >= scroll + area_height {
            scroll = cursor_line + 1 - area_height;
        }
        scroll
    }

    /// Screen cell of the cursor inside `area`, honouring wrapping and scroll.
    pub fn cursor_pos_with_state(&self, area: Rect, state: TextAreaState) -> Option<(u16, u16)> {
        if area.is_empty() {
            return None;
        }
        let lines = self.wrapped_lines(area.width);
        let scroll = self.effective_scroll(area.height, &lines, state.scroll);
        let i = Self::wrapped_line_index_by_start(&lines, self.cursor_pos)?;
        let ls = &lines[i];
        let col = width_of(&self.text[ls.start..self.cursor_pos])
            .min(usize::from(area.width.saturating_sub(1))) as u16;
        let row = i.saturating_sub(scroll as usize) as u16;
        Some((area.x + col, area.y + row))
    }

    /// Draw with `base` under everything, elements in cyan, then `highlights` on top.
    pub fn render(
        &self,
        area: Rect,
        buf: &mut Buffer,
        state: &mut TextAreaState,
        base: Style,
        highlights: &[(Range<usize>, Style)],
    ) {
        if area.is_empty() {
            return;
        }
        let lines = self.wrapped_lines(area.width);
        let scroll = self.effective_scroll(area.height, &lines, state.scroll);
        state.scroll = scroll;
        let start = scroll as usize;
        let end = (scroll + area.height).min(lines.len() as u16) as usize;
        for (row, idx) in (start..end).enumerate() {
            let r = &lines[idx];
            let y = area.y + row as u16;
            let line_range = r.start..r.end - 1;
            buf.set_style(Rect::new(area.x, y, area.width, 1), base);
            buf.set_stringn(
                area.x,
                y,
                text_for_display(&self.text[line_range.clone()]),
                usize::from(area.width),
                base,
            );
            let overlay = |range: &Range<usize>, style: Style, buf: &mut Buffer| {
                let s = range.start.max(line_range.start);
                let e = range.end.min(line_range.end);
                if s >= e {
                    return;
                }
                let x_off = width_of(&self.text[line_range.start..s]) as u16;
                buf.set_stringn(
                    area.x + x_off,
                    y,
                    text_for_display(&self.text[s..e]),
                    usize::from(area.width.saturating_sub(x_off)),
                    style,
                );
            };
            for e in &self.elements {
                overlay(&e.range, base.fg(Color::Cyan), buf);
            }
            for (range, style) in highlights {
                overlay(range, *style, buf);
            }
        }
    }
}

/// Wrap rows for `display` at `width`, in Codex's sentinel-range form.
fn wrap_rows(display: &str, width: u16) -> Vec<Range<usize>> {
    let w = width as usize;
    // Pass 1: textwrap-shaped wrapping per logical line, each row extended over the spaces that
    // follow it plus one sentinel byte.
    let mut first: Vec<Range<usize>> = Vec::new();
    let mut offset = 0usize;
    for logical in display.split('\n') {
        let rows = wrap_ranges_trim(logical, w.max(1), true, true);
        for r in rows {
            let start = offset + r.start;
            let end = offset + r.end;
            let trailing = display[end..].chars().take_while(|c| *c == ' ').count();
            first.push(start..end + trailing + 1);
        }
        offset += logical.len() + 1;
    }
    if w == 0 {
        return first;
    }
    // Pass 2: spaces that overflow the width move to a row of their own; an exactly full row at
    // a line break or the end of the text gets an empty row after it.
    let mut lines = Vec::with_capacity(first.len());
    for wl in first {
        let line_end = wl.end.saturating_sub(1);
        let mut line_start = wl.start;
        let mut line_width = 0usize;
        for (off, g) in display[wl.start..line_end].grapheme_indices(true) {
            let gw = width_of(g);
            if line_width > 0 && line_width + gw > w {
                let next = wl.start + off;
                lines.push(line_start..next + 1);
                line_start = next;
                line_width = 0;
            }
            line_width += gw;
        }
        lines.push(line_start..line_end + 1);
        if line_width >= w && matches!(display.as_bytes().get(line_end), None | Some(b'\n')) {
            lines.push(line_end..line_end + 1);
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Style;

    fn ta(text: &str) -> TextArea {
        let mut t = TextArea::new();
        t.insert_str(text);
        t
    }

    fn key(c: char, m: KeyModifiers) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), m)
    }

    fn rows(t: &TextArea, w: u16) -> Vec<String> {
        let lines = t.wrapped_lines(w);
        lines
            .iter()
            .map(|r| t.text[r.start..r.end - 1].to_string())
            .collect()
    }

    #[test]
    fn insert_and_replace_update_cursor() {
        let mut t = ta("abc");
        assert_eq!(t.cursor(), 3);
        t.set_cursor(1);
        t.insert_str("XY");
        assert_eq!(t.text(), "aXYbc");
        assert_eq!(t.cursor(), 3);
        t.replace_range(0..2, "_");
        assert_eq!(t.text(), "_Ybc");
        assert_eq!(t.cursor(), 2);
    }

    #[test]
    fn exactly_full_line_gets_a_cursor_row() {
        let t = ta(&"a".repeat(10));
        assert_eq!(t.desired_height(10), 2);
        let t = ta(&"a".repeat(9));
        assert_eq!(t.desired_height(10), 1);
        // A full row followed by a space and more text also leaves an empty row.
        let t = ta(&format!("{} bb", "a".repeat(10)));
        assert_eq!(
            rows(&t, 10),
            vec!["a".repeat(10), " ".to_string(), "bb".to_string()]
        );
    }

    #[test]
    fn wraps_at_words_and_splits_long_words() {
        let t = ta("hello wonderful world");
        assert_eq!(rows(&t, 10), vec!["hello ", "wonderful ", "world"]);
        let t = ta("abcdefghijkl");
        assert_eq!(rows(&t, 5), vec!["abcde", "fghij", "kl"]);
    }

    #[test]
    fn tabs_count_one_column() {
        let t = ta("a\tb");
        assert_eq!(t.desired_height(3), 2);
        assert_eq!(t.desired_height(4), 1);
    }

    #[test]
    fn cursor_pos_follows_wrapping_and_scroll() {
        let mut t = ta("one\ntwo\nthree");
        let area = Rect::new(2, 5, 10, 2);
        assert_eq!(
            t.cursor_pos_with_state(area, TextAreaState::default()),
            Some((7, 6))
        );
        t.set_cursor(0);
        assert_eq!(
            t.cursor_pos_with_state(area, TextAreaState::default()),
            Some((2, 5))
        );
    }

    #[test]
    fn elements_are_atomic() {
        let mut t = TextArea::new();
        t.insert_str("a ");
        t.insert_element("[Pasted Content 1500 chars]");
        t.insert_str(" b");
        t.set_cursor(2 + 5);
        // The cursor snaps out of the element.
        assert!(t.cursor() == 2 || t.cursor() == 2 + 27);
        t.set_cursor(2 + 27);
        t.delete_backward(1);
        assert_eq!(t.text(), "a  b");
    }

    #[test]
    fn typing_inside_an_element_lands_at_its_edge() {
        let mut t = TextArea::new();
        t.insert_element("[X]");
        t.insert_str_at(1, "z");
        assert!(t.text() == "z[X]" || t.text() == "[X]z");
    }

    #[test]
    fn delete_forward_removes_whole_element() {
        let mut t = TextArea::new();
        t.insert_element("[Image #1]");
        t.set_cursor(0);
        t.delete_forward(1);
        assert_eq!(t.text(), "");
    }

    #[test]
    fn kill_and_yank_survive_set_text() {
        let mut t = ta("hello world");
        t.set_cursor(5);
        t.kill_to_end_of_line();
        assert_eq!(t.text(), "hello");
        t.set_text_clearing_elements("");
        t.yank();
        assert_eq!(t.text(), " world");
    }

    #[test]
    fn kill_at_line_end_takes_the_newline() {
        let mut t = ta("a\nb");
        t.set_cursor(1);
        t.kill_to_end_of_line();
        assert_eq!(t.text(), "ab");
    }

    #[test]
    fn ctrl_u_at_line_start_joins_lines() {
        let mut t = ta("a\nb");
        t.set_cursor(2);
        t.kill_to_beginning_of_line();
        assert_eq!(t.text(), "ab");
    }

    #[test]
    fn word_motions_respect_separators() {
        let mut t = ta("foo/bar baz");
        assert_eq!(t.beginning_of_previous_word(), 8);
        t.set_cursor(8);
        assert_eq!(t.beginning_of_previous_word(), 4);
        t.set_cursor(0);
        assert_eq!(t.end_of_next_word(), 3);
        t.set_cursor(3);
        assert_eq!(t.end_of_next_word(), 4);
    }

    #[test]
    fn editing_keys_match_the_capture() {
        // xg-09: `hello world foo bar`, ctrl+a X, ctrl+e Y, alt+b Z, ctrl+w, ctrl+k, ctrl+y, ctrl+h.
        let mut t = ta("hello world foo bar");
        t.input(key('a', KeyModifiers::CONTROL), 80);
        t.input(key('X', KeyModifiers::SHIFT), 80);
        assert_eq!(t.text(), "Xhello world foo bar");
        t.input(key('e', KeyModifiers::CONTROL), 80);
        t.input(key('Y', KeyModifiers::SHIFT), 80);
        t.input(key('b', KeyModifiers::ALT), 80);
        t.input(key('Z', KeyModifiers::SHIFT), 80);
        assert_eq!(t.text(), "Xhello world foo ZbarY");
        t.input(key('w', KeyModifiers::CONTROL), 80);
        assert_eq!(t.text(), "Xhello world foo barY");
        t.input(key('e', KeyModifiers::CONTROL), 80);
        t.input(key('h', KeyModifiers::CONTROL), 80);
        assert_eq!(t.text(), "Xhello world foo bar");
    }

    #[test]
    fn up_down_keep_the_column_across_wrapped_rows() {
        let mut t = ta("abcdef\nxy\nabcdef");
        t.set_cursor(5);
        t.move_cursor_down(40);
        assert_eq!(t.cursor(), 9);
        t.move_cursor_down(40);
        assert_eq!(t.cursor(), 15);
        t.move_cursor_up(40);
        t.move_cursor_up(40);
        assert_eq!(t.cursor(), 5);
        t.move_cursor_up(40);
        assert_eq!(t.cursor(), 0);
    }

    #[test]
    fn render_styles_elements_cyan() {
        let mut t = TextArea::new();
        t.insert_str("a ");
        t.insert_element("[Pasted Content 1500 chars]");
        let area = Rect::new(0, 0, 40, 1);
        let mut buf = Buffer::empty(area);
        let mut st = TextAreaState::default();
        t.render(area, &mut buf, &mut st, Style::default(), &[]);
        assert_eq!(buf[(0, 0)].fg, Color::Reset);
        assert_eq!(buf[(2, 0)].fg, Color::Cyan);
        assert_eq!(buf[(2, 0)].symbol(), "[");
    }

    #[test]
    fn scroll_keeps_cursor_visible() {
        let t = ta("1\n2\n3\n4\n5");
        let area = Rect::new(0, 0, 10, 3);
        let mut buf = Buffer::empty(area);
        let mut st = TextAreaState::default();
        t.render(area, &mut buf, &mut st, Style::default(), &[]);
        assert_eq!(st.scroll, 2);
        assert_eq!(buf[(0, 0)].symbol(), "3");
        assert_eq!(t.cursor_pos_with_state(area, st), Some((1, 2)));
    }
}
