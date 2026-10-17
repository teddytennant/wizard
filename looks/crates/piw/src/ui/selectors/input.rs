//! pi-tui's `Input`: one line, cursor, `> ` prompt, reverse-video cursor cell.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;

use crate::ui::span;

#[derive(Clone, Debug, Default)]
pub struct Input {
    text: String,
    cursor: usize,
}

impl Input {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(text: &str) -> Self {
        Input {
            text: text.to_string(),
            cursor: text.len(),
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Byte offset of the cursor.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn set(&mut self, s: &str) {
        self.text = s.to_string();
        self.cursor = self.text.len();
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    pub fn paste(&mut self, s: &str) {
        let clean: String = s.chars().filter(|c| !c.is_control()).collect();
        self.text.insert_str(self.cursor, &clean);
        self.cursor += clean.len();
    }

    fn prev(&self) -> usize {
        self.text[..self.cursor]
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(i, _)| i)
    }

    fn next(&self) -> usize {
        self.text[self.cursor..]
            .graphemes(true)
            .next()
            .map_or(self.cursor, |g| self.cursor + g.len())
    }

    fn word_back(&self) -> usize {
        let t = &self.text[..self.cursor];
        let t2 = t.trim_end();
        t2.rfind(char::is_whitespace).map_or(0, |i| i + 1)
    }

    /// Returns `true` when the key edited or moved something. Enter, Escape, Tab and the
    /// up/down keys are left to the caller.
    pub fn key(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Char('u') if ctrl => {
                self.text.drain(..self.cursor);
                self.cursor = 0;
            }
            KeyCode::Char('k') if ctrl => self.text.truncate(self.cursor),
            KeyCode::Char('w') if ctrl => {
                let a = self.word_back();
                self.text.drain(a..self.cursor);
                self.cursor = a;
            }
            KeyCode::Char('a') if ctrl => self.cursor = 0,
            KeyCode::Char('e') if ctrl => self.cursor = self.text.len(),
            KeyCode::Char('b') if ctrl => self.cursor = self.prev(),
            KeyCode::Char('f') if ctrl => self.cursor = self.next(),
            KeyCode::Char(c) if !ctrl && !alt => {
                self.text.insert(self.cursor, c);
                self.cursor += c.len_utf8();
            }
            KeyCode::Backspace => {
                let a = self.prev();
                self.text.drain(a..self.cursor);
                self.cursor = a;
            }
            KeyCode::Delete => {
                let b = self.next();
                self.text.drain(self.cursor..b);
            }
            KeyCode::Left => self.cursor = self.prev(),
            KeyCode::Right => self.cursor = self.next(),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.text.len(),
            _ => return false,
        }
        true
    }

    /// `> text▮`, scrolled horizontally when it does not fit `width`.
    pub fn render(&self, width: u16) -> Line<'static> {
        let rev = Style::default().add_modifier(Modifier::REVERSED);
        let avail = (width as usize).saturating_sub(3).max(1);
        let before = &self.text[..self.cursor];
        let after = &self.text[self.cursor..];
        let bw = tuikit::width::display_width(before);
        let before: String = if bw >= avail {
            super::cut_left(before, avail - 1)
        } else {
            before.to_string()
        };
        let mut spans: Vec<Span<'static>> = vec![Span::raw("> "), Span::raw(before)];
        match after.graphemes(true).next() {
            Some(g) => {
                spans.push(span(g.to_string(), rev));
                spans.push(Span::raw(after[g.len()..].to_string()));
            }
            None => spans.push(span(" ", rev)),
        }
        Line::from(spans)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    #[test]
    fn typing_moving_and_deleting() {
        let mut i = Input::new();
        for c in "abc".chars() {
            i.key(k(KeyCode::Char(c)));
        }
        i.key(k(KeyCode::Left));
        i.key(k(KeyCode::Backspace));
        assert_eq!(i.text(), "ac");
        i.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(i.text(), "c");
    }
}
