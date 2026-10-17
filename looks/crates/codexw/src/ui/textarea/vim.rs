// OWNER: bottom-pane
//! Vim editing for the composer (`/vim`, spec C.2.8), default bindings only: normal mode with
//! motions, the `d`, `y` and `c` operators, text objects, and insert mode through the regular
//! editor keys. Ported from Codex's `textarea/vim.rs` and the Vim half of `textarea.rs`.

use std::ops::Range;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use super::{KillBufferKind, TextArea, split_word_pieces};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VimMode {
    Normal,
    Insert,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VimOperator {
    Delete,
    Yank,
    Change,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VimPending {
    None,
    Operator(VimOperator),
    TextObject { operator: VimOperator, scope: Scope },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Inner,
    Around,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VimMotion {
    Left,
    Right,
    Up,
    Down,
    WordForward,
    WordBackward,
    WordEnd,
    LineStart,
    LineEnd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VimTextObject {
    Word,
    BigWord,
    Parentheses,
    Brackets,
    Braces,
    DoubleQuote,
    SingleQuote,
    Backtick,
}

#[derive(Debug)]
pub struct VimState {
    pub enabled: bool,
    pub mode: VimMode,
    pub pending: VimPending,
}

impl Default for VimState {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: VimMode::Insert,
            pending: VimPending::None,
        }
    }
}

fn ch_of(e: KeyEvent) -> Option<char> {
    match e.code {
        KeyCode::Char(c)
            if !e
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            Some(c)
        }
        _ => None,
    }
}

impl TextArea {
    /// Turn Vim on (normal mode) or off (plain insert semantics), dropping any pending operator.
    pub fn set_vim_enabled(&mut self, enabled: bool) {
        self.vim.enabled = enabled;
        self.vim.pending = VimPending::None;
        self.vim.mode = if enabled {
            VimMode::Normal
        } else {
            VimMode::Insert
        };
    }

    pub fn is_vim_enabled(&self) -> bool {
        self.vim.enabled
    }

    pub fn is_vim_normal_mode(&self) -> bool {
        self.vim.enabled && self.vim.mode == VimMode::Normal
    }

    pub fn is_vim_operator_pending(&self) -> bool {
        !matches!(self.vim.pending, VimPending::None)
    }

    pub fn enter_vim_insert_mode(&mut self) {
        if self.vim.enabled {
            self.vim.mode = VimMode::Insert;
            self.vim.pending = VimPending::None;
        }
    }

    pub fn enter_vim_normal_mode(&mut self) {
        if self.vim.enabled {
            self.vim.mode = VimMode::Normal;
            self.vim.pending = VimPending::None;
            self.preferred_col = None;
        }
    }

    /// Fast plain keys are commands in normal mode, never a paste burst.
    pub fn allows_paste_burst(&self) -> bool {
        !self.vim.enabled || self.vim.mode == VimMode::Insert
    }

    pub fn uses_vim_insert_cursor(&self) -> bool {
        self.vim.enabled && self.vim.mode == VimMode::Insert
    }

    /// Esc in insert mode leaves it for normal mode; the composer must not use it first.
    pub fn should_handle_vim_insert_escape(&self, event: KeyEvent) -> bool {
        self.vim.enabled
            && self.vim.mode == VimMode::Insert
            && event.code == KeyCode::Esc
            && event.modifiers == KeyModifiers::NONE
            && matches!(event.kind, KeyEventKind::Press | KeyEventKind::Repeat)
    }

    pub fn vim_mode_label(&self) -> Option<&'static str> {
        if !self.vim.enabled {
            return None;
        }
        Some(match self.vim.mode {
            VimMode::Normal => "Normal",
            VimMode::Insert => "Insert",
        })
    }

    pub(crate) fn handle_vim_input(&mut self, event: KeyEvent, width: u16) {
        match self.vim.mode {
            VimMode::Insert => {
                if matches!(event.code, KeyCode::Esc) {
                    let bol = self.beginning_of_current_line();
                    if self.cursor_pos > bol {
                        self.cursor_pos = self.prev_atomic_boundary(self.cursor_pos).max(bol);
                    }
                    self.enter_vim_normal_mode();
                } else {
                    self.input_insert_mode(event, width);
                }
            }
            VimMode::Normal => self.handle_vim_normal(event, width),
        }
    }

    fn handle_vim_normal(&mut self, event: KeyEvent, width: u16) {
        let pending = std::mem::replace(&mut self.vim.pending, VimPending::None);
        match pending {
            VimPending::None => {}
            VimPending::Operator(op) => {
                self.handle_vim_operator(op, event, width);
                return;
            }
            VimPending::TextObject { operator, scope } => {
                self.handle_vim_text_object(operator, scope, event);
                return;
            }
        }
        let arrow = |c: KeyCode| event.code == c && event.modifiers == KeyModifiers::NONE;
        if arrow(KeyCode::Left) {
            self.move_cursor_left();
            return;
        }
        if arrow(KeyCode::Right) {
            self.move_cursor_right();
            return;
        }
        if arrow(KeyCode::Down) {
            self.move_cursor_down(width);
            return;
        }
        if arrow(KeyCode::Up) {
            self.move_cursor_up(width);
            return;
        }
        if arrow(KeyCode::Insert) {
            self.vim.mode = VimMode::Insert;
            return;
        }
        if arrow(KeyCode::Esc) {
            return;
        }
        let Some(c) = ch_of(event) else {
            return;
        };
        match c {
            'i' => self.vim.mode = VimMode::Insert,
            'a' => {
                let next = self.next_atomic_boundary(self.cursor_pos);
                self.set_cursor(next);
                self.vim.mode = VimMode::Insert;
            }
            'A' => {
                self.set_cursor(self.end_of_current_line());
                self.vim.mode = VimMode::Insert;
            }
            'I' => {
                self.set_cursor(self.first_non_blank_of_current_line());
                self.vim.mode = VimMode::Insert;
            }
            'o' => {
                let eol = self.end_of_current_line();
                let old_len = self.text.len();
                let insert_at = if eol < old_len { eol + 1 } else { eol };
                self.insert_str_at(insert_at, "\n");
                let cursor = if eol < old_len {
                    insert_at
                } else {
                    insert_at + 1
                };
                self.set_cursor(cursor);
                self.vim.mode = VimMode::Insert;
            }
            'O' => {
                let bol = self.beginning_of_current_line();
                self.insert_str_at(bol, "\n");
                self.set_cursor(bol);
                self.vim.mode = VimMode::Insert;
            }
            'h' => self.move_cursor_left(),
            'l' => self.move_cursor_right(),
            'j' => self.move_cursor_down(width),
            'k' => self.move_cursor_up(width),
            'w' => self.set_cursor(self.beginning_of_next_word()),
            'b' => self.set_cursor(self.beginning_of_previous_word()),
            'e' => self.set_cursor(self.vim_word_end_cursor()),
            '0' => self.set_cursor(self.beginning_of_current_line()),
            '$' => self.set_cursor(self.vim_line_end_cursor()),
            'x' => self.delete_forward_kill(1),
            's' => {
                if self.cursor_pos < self.end_of_current_line() {
                    self.delete_forward_kill(1);
                }
                self.vim.mode = VimMode::Insert;
            }
            'D' => self.vim_kill_to_end_of_line(),
            'C' => {
                self.vim_kill_to_end_of_line();
                self.vim.mode = VimMode::Insert;
            }
            'Y' => self.yank_current_line(),
            'p' => self.paste_after_cursor(),
            'd' => self.vim.pending = VimPending::Operator(VimOperator::Delete),
            'y' => self.vim.pending = VimPending::Operator(VimOperator::Yank),
            'c' => self.vim.pending = VimPending::Operator(VimOperator::Change),
            _ => {}
        }
    }

    fn handle_vim_operator(&mut self, op: VimOperator, event: KeyEvent, width: u16) {
        if event.code == KeyCode::Esc {
            return;
        }
        let Some(c) = ch_of(event) else {
            return;
        };
        if op == VimOperator::Delete && c == 'd' {
            self.kill_current_line();
            return;
        }
        if op == VimOperator::Yank && c == 'y' {
            self.yank_current_line();
            return;
        }
        let scope = match c {
            'i' => Some(Scope::Inner),
            'a' => Some(Scope::Around),
            _ => None,
        };
        if let Some(scope) = scope {
            self.vim.pending = VimPending::TextObject {
                operator: op,
                scope,
            };
            return;
        }
        if op != VimOperator::Change {
            let motion = match c {
                'h' => Some(VimMotion::Left),
                'l' => Some(VimMotion::Right),
                'j' => Some(VimMotion::Down),
                'k' => Some(VimMotion::Up),
                'w' => Some(VimMotion::WordForward),
                'b' => Some(VimMotion::WordBackward),
                'e' => Some(VimMotion::WordEnd),
                '0' => Some(VimMotion::LineStart),
                '$' => Some(VimMotion::LineEnd),
                _ => None,
            };
            if let Some(m) = motion {
                self.apply_vim_operator(op, m, width);
            }
        }
    }

    fn handle_vim_text_object(&mut self, op: VimOperator, scope: Scope, event: KeyEvent) {
        if event.code == KeyCode::Esc {
            return;
        }
        let Some(c) = ch_of(event) else {
            return;
        };
        let object = match c {
            'w' => VimTextObject::Word,
            'W' => VimTextObject::BigWord,
            '(' | ')' | 'b' => VimTextObject::Parentheses,
            '[' | ']' => VimTextObject::Brackets,
            '{' | '}' | 'B' => VimTextObject::Braces,
            '"' => VimTextObject::DoubleQuote,
            '\'' => VimTextObject::SingleQuote,
            '`' => VimTextObject::Backtick,
            _ => return,
        };
        if let Some(range) = self.text_object_range(object, scope) {
            self.apply_vim_operator_to_range(op, range);
        }
    }

    fn apply_vim_operator(&mut self, op: VimOperator, motion: VimMotion, width: u16) {
        let Some(range) = self.range_for_motion(motion, width) else {
            return;
        };
        match op {
            VimOperator::Delete => self.kill_range(range),
            VimOperator::Yank => self.yank_range_with_kind(range, KillBufferKind::Characterwise),
            VimOperator::Change => {}
        }
    }

    fn apply_vim_operator_to_range(&mut self, op: VimOperator, range: Range<usize>) {
        match op {
            VimOperator::Delete => self.kill_range(range),
            VimOperator::Yank => self.yank_range_with_kind(range, KillBufferKind::Characterwise),
            VimOperator::Change => {
                self.kill_range(range);
                self.vim.mode = VimMode::Insert;
            }
        }
    }

    fn range_for_motion(&mut self, motion: VimMotion, width: u16) -> Option<Range<usize>> {
        if matches!(motion, VimMotion::Up | VimMotion::Down) {
            return self.linewise_range_for_vertical_motion(motion);
        }
        let start = self.cursor_pos;
        let target = self.target_for_motion(motion, width);
        if start == target {
            return None;
        }
        Some(start.min(target)..start.max(target))
    }

    fn linewise_range_for_vertical_motion(&self, motion: VimMotion) -> Option<Range<usize>> {
        let current = self.current_line_range_with_newline();
        let range = match motion {
            VimMotion::Up => {
                let start = if current.start == 0 {
                    current.start
                } else {
                    self.beginning_of_line(current.start.saturating_sub(1))
                };
                start..current.end
            }
            VimMotion::Down => {
                let end = if current.end >= self.text.len() {
                    current.end
                } else {
                    let next_eol = self.end_of_line(current.end);
                    if next_eol < self.text.len() {
                        next_eol + 1
                    } else {
                        next_eol
                    }
                };
                current.start..end
            }
            _ => return None,
        };
        (range.start < range.end).then_some(range)
    }

    fn target_for_motion(&mut self, motion: VimMotion, width: u16) -> usize {
        let original_cursor = self.cursor_pos;
        let original_preferred = self.preferred_col;
        match motion {
            VimMotion::Left => self.move_cursor_left(),
            VimMotion::Right => self.move_cursor_right(),
            VimMotion::Up => self.move_cursor_up(width),
            VimMotion::Down => self.move_cursor_down(width),
            VimMotion::WordForward => self.set_cursor(self.beginning_of_next_word()),
            VimMotion::WordBackward => self.set_cursor(self.beginning_of_previous_word()),
            VimMotion::WordEnd => self.set_cursor(self.vim_word_end_exclusive()),
            VimMotion::LineStart => self.set_cursor(self.beginning_of_current_line()),
            VimMotion::LineEnd => self.set_cursor(self.end_of_current_line()),
        }
        let target = self.cursor_pos;
        self.cursor_pos = original_cursor;
        self.preferred_col = original_preferred;
        target
    }

    pub(crate) fn delete_forward_kill(&mut self, n: usize) {
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
        self.kill_range(self.cursor_pos..target);
    }

    fn vim_kill_to_end_of_line(&mut self) {
        let eol = self.end_of_current_line();
        if self.cursor_pos < eol {
            self.kill_range(self.cursor_pos..eol);
        }
    }

    fn paste_after_cursor(&mut self) {
        if self.kill_buffer.is_empty() {
            return;
        }
        if self.kill_buffer_kind == KillBufferKind::Linewise {
            self.paste_line_after_current_line();
            return;
        }
        let insert_at = self.next_atomic_boundary(self.cursor_pos);
        self.set_cursor(insert_at);
        let text = self.kill_buffer.clone();
        self.insert_str(&text);
    }

    fn paste_line_after_current_line(&mut self) {
        let eol = self.end_of_current_line();
        let insert_at = if eol < self.text.len() { eol + 1 } else { eol };
        let cursor = if eol < self.text.len() {
            insert_at
        } else {
            insert_at + 1
        };
        let text = if eol < self.text.len() {
            if self.kill_buffer.ends_with('\n') {
                self.kill_buffer.clone()
            } else {
                format!("{}\n", self.kill_buffer)
            }
        } else {
            format!("\n{}", self.kill_buffer.trim_end_matches('\n'))
        };
        self.insert_str_at(insert_at, &text);
        self.set_cursor(cursor.min(self.text.len()));
    }

    fn yank_current_line(&mut self) {
        let range = self.current_line_range_with_newline();
        self.yank_range_with_kind(range, KillBufferKind::Linewise);
    }

    pub(crate) fn kill_current_line(&mut self) {
        let range = self.current_line_range_with_newline();
        self.kill_range_with_kind(range, KillBufferKind::Linewise);
    }

    fn current_line_range_with_newline(&self) -> Range<usize> {
        let bol = self.beginning_of_current_line();
        let eol = self.end_of_current_line();
        let end = if eol < self.text.len() { eol + 1 } else { eol };
        bol..end
    }

    fn vim_word_end_exclusive(&self) -> usize {
        let end = self.end_of_next_word();
        let target = if end > self.cursor_pos {
            self.prev_atomic_boundary(end)
        } else {
            end
        };
        if target == self.cursor_pos && end < self.text.len() {
            self.end_of_next_word_from(end)
        } else {
            end
        }
    }

    fn vim_word_end_cursor(&self) -> usize {
        let end = self.vim_word_end_exclusive();
        if end > self.cursor_pos {
            self.prev_atomic_boundary(end)
        } else {
            end
        }
    }

    fn vim_line_end_cursor(&self) -> usize {
        let bol = self.beginning_of_current_line();
        let eol = self.end_of_current_line();
        if eol > bol {
            self.prev_atomic_boundary(eol).max(bol)
        } else {
            eol
        }
    }

    // ---- text objects ---------------------------------------------------------------------

    fn text_object_range(&self, object: VimTextObject, scope: Scope) -> Option<Range<usize>> {
        match object {
            VimTextObject::Word => self.word_text_object_range(scope, false),
            VimTextObject::BigWord => self.word_text_object_range(scope, true),
            VimTextObject::Parentheses => self.paired_text_object_range(scope, '(', ')'),
            VimTextObject::Brackets => self.paired_text_object_range(scope, '[', ']'),
            VimTextObject::Braces => self.paired_text_object_range(scope, '{', '}'),
            VimTextObject::DoubleQuote => self.quoted_text_object_range(scope, '"'),
            VimTextObject::SingleQuote => self.quoted_text_object_range(scope, '\''),
            VimTextObject::Backtick => self.quoted_text_object_range(scope, '`'),
        }
    }

    fn word_text_object_range(&self, scope: Scope, big_word: bool) -> Option<Range<usize>> {
        let inner = if big_word {
            self.big_word_range_at_cursor()?
        } else {
            self.small_word_range_at_cursor()?
        };
        Some(match scope {
            Scope::Inner => inner,
            Scope::Around => self.expand_word_around(inner),
        })
    }

    fn big_word_range_at_cursor(&self) -> Option<Range<usize>> {
        self.non_ws_runs()
            .into_iter()
            .find(|r| self.cursor_overlaps_range(r) || self.cursor_is_at_range_end(r))
    }

    fn small_word_range_at_cursor(&self) -> Option<Range<usize>> {
        for run in self.non_ws_runs() {
            if !self.cursor_overlaps_range(&run) && !self.cursor_is_at_range_end(&run) {
                continue;
            }
            let mut last_piece = None;
            for (piece_start, piece) in split_word_pieces(&self.text[run.clone()]) {
                let piece = run.start + piece_start..run.start + piece_start + piece.len();
                if self.cursor_overlaps_range(&piece) {
                    return Some(piece);
                }
                last_piece = Some(piece);
            }
            if self.cursor_is_at_range_end(&run) {
                return last_piece.or(Some(run));
            }
            return Some(run);
        }
        None
    }

    fn non_ws_runs(&self) -> Vec<Range<usize>> {
        let mut runs = Vec::new();
        let mut start = None;
        for (idx, ch) in self.text.char_indices() {
            if ch.is_whitespace() {
                if let Some(s) = start.take() {
                    runs.push(s..idx);
                }
            } else if start.is_none() {
                start = Some(idx);
            }
        }
        if let Some(s) = start {
            runs.push(s..self.text.len());
        }
        runs
    }

    fn cursor_overlaps_range(&self, r: &Range<usize>) -> bool {
        r.start <= self.cursor_pos && self.cursor_pos < r.end
    }

    fn cursor_is_at_range_end(&self, r: &Range<usize>) -> bool {
        r.start < r.end && self.cursor_pos == r.end
    }

    fn expand_word_around(&self, inner: Range<usize>) -> Range<usize> {
        let mut end = inner.end;
        for (off, ch) in self.text[inner.end..].char_indices() {
            if !ch.is_whitespace() {
                break;
            }
            end = inner.end + off + ch.len_utf8();
        }
        if end > inner.end {
            return inner.start..end;
        }
        let mut start = inner.start;
        for (idx, ch) in self.text[..inner.start].char_indices().rev() {
            if !ch.is_whitespace() {
                break;
            }
            start = idx;
        }
        start..inner.end
    }

    fn paired_text_object_range(
        &self,
        scope: Scope,
        open: char,
        close: char,
    ) -> Option<Range<usize>> {
        let mut stack: Vec<usize> = Vec::new();
        let mut best: Option<Range<usize>> = None;
        for (idx, ch) in self.text.char_indices() {
            if self.is_inside_element(idx) {
                continue;
            }
            if ch == open {
                stack.push(idx);
            } else if ch == close {
                let Some(open_idx) = stack.pop() else {
                    continue;
                };
                let close_end = idx + ch.len_utf8();
                if open_idx <= self.cursor_pos && self.cursor_pos <= idx {
                    let cand = match scope {
                        Scope::Inner => open_idx + open.len_utf8()..idx,
                        Scope::Around => open_idx..close_end,
                    };
                    if cand.start <= cand.end && best.as_ref().is_none_or(|c| cand.len() < c.len())
                    {
                        best = Some(cand);
                    }
                }
            }
        }
        best
    }

    fn quoted_text_object_range(&self, scope: Scope, quote: char) -> Option<Range<usize>> {
        let line = self.beginning_of_current_line()..self.end_of_current_line();
        let mut open = None;
        let mut best: Option<Range<usize>> = None;
        for (offset, ch) in self.text[line.clone()].char_indices() {
            let idx = line.start + offset;
            if self.is_inside_element(idx) || ch != quote || self.is_escaped(idx) {
                continue;
            }
            if let Some(open_idx) = open.take() {
                if open_idx <= self.cursor_pos && self.cursor_pos <= idx {
                    let cand = match scope {
                        Scope::Inner => open_idx + quote.len_utf8()..idx,
                        Scope::Around => open_idx..idx + quote.len_utf8(),
                    };
                    if cand.start <= cand.end && best.as_ref().is_none_or(|c| cand.len() < c.len())
                    {
                        best = Some(cand);
                    }
                }
            } else {
                open = Some(idx);
            }
        }
        best
    }

    fn is_inside_element(&self, pos: usize) -> bool {
        self.elements
            .iter()
            .any(|e| pos >= e.range.start && pos < e.range.end)
    }

    fn is_escaped(&self, pos: usize) -> bool {
        let mut n = 0;
        for ch in self.text[..pos].chars().rev() {
            if ch != '\\' {
                break;
            }
            n += 1;
        }
        n % 2 == 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn esc() -> KeyEvent {
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)
    }

    fn vim() -> TextArea {
        let mut t = TextArea::new();
        t.set_vim_enabled(true);
        t
    }

    fn type_keys(t: &mut TextArea, keys: &str) {
        for c in keys.chars() {
            t.input(k(c), 80);
        }
    }

    #[test]
    fn starts_in_normal_and_i_inserts() {
        let mut t = vim();
        assert_eq!(t.vim_mode_label(), Some("Normal"));
        type_keys(&mut t, "ihello");
        assert_eq!(t.text(), "hello");
        assert_eq!(t.vim_mode_label(), Some("Insert"));
    }

    #[test]
    fn capture_x_dd_p() {
        // xg-13: `i hello`, Esc, `0`, `x` leaves `ello`; `dd` clears; `p` pastes the line below.
        let mut t = vim();
        type_keys(&mut t, "ihello");
        t.input(esc(), 80);
        type_keys(&mut t, "0x");
        assert_eq!(t.text(), "ello");
        type_keys(&mut t, "dd");
        assert_eq!(t.text(), "");
        type_keys(&mut t, "p");
        assert_eq!(t.text(), "\nello");
    }

    #[test]
    fn escape_steps_back_one_grapheme() {
        let mut t = vim();
        type_keys(&mut t, "iab");
        t.input(esc(), 80);
        assert_eq!(t.cursor(), 1);
        assert_eq!(t.vim_mode_label(), Some("Normal"));
    }

    #[test]
    fn change_inner_word_enters_insert() {
        let mut t = vim();
        type_keys(&mut t, "ifoo bar");
        t.input(esc(), 80);
        type_keys(&mut t, "0ciw");
        assert_eq!(t.text(), " bar");
        assert_eq!(t.vim_mode_label(), Some("Insert"));
    }

    #[test]
    fn delete_to_end_and_o_open_line() {
        let mut t = vim();
        type_keys(&mut t, "iab cd");
        t.input(esc(), 80);
        type_keys(&mut t, "0wD");
        assert_eq!(t.text(), "ab ");
        type_keys(&mut t, "oX");
        assert_eq!(t.text(), "ab \nX");
    }

    #[test]
    fn quote_object_is_line_local() {
        let mut t = vim();
        type_keys(&mut t, "ia \"b c\" d");
        t.input(esc(), 80);
        t.set_cursor(4);
        type_keys(&mut t, "di\"");
        assert_eq!(t.text(), "a \"\" d");
    }
}
