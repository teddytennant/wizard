//! The one-line prompt view (`CustomPromptView`, spec C.7.5): a cyan `▌` gutter on every row,
//! a bold title, an input area with a dim placeholder, and the standard hint at column 0.
//! `/rename` uses it. Unlike the selection lists it has no tinted band.

use std::fmt;
use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::Stylize;
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;

use crate::ui::{AppAction, BottomView, ViewResult};
use crate::wrap::width_of;

pub type Submit = Arc<dyn Fn(String) -> AppAction + Send + Sync>;

pub struct PromptView {
    title: String,
    placeholder: String,
    context_label: Option<String>,
    text: String,
    /// Cursor as a grapheme index.
    cursor: usize,
    submit: Submit,
}

impl fmt::Debug for PromptView {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PromptView")
            .field("title", &self.title)
            .field("text", &self.text)
            .finish()
    }
}

fn gutter() -> Span<'static> {
    "▌ ".cyan()
}

impl PromptView {
    pub fn new(
        title: &str,
        placeholder: &str,
        context_label: Option<String>,
        submit: Submit,
    ) -> Self {
        Self {
            title: title.into(),
            placeholder: placeholder.into(),
            context_label,
            text: String::new(),
            cursor: 0,
            submit,
        }
    }

    fn graphemes(&self) -> Vec<&str> {
        self.text.graphemes(true).collect()
    }

    fn insert(&mut self, s: &str) {
        let mut g: Vec<String> = self.graphemes().iter().map(|x| x.to_string()).collect();
        let new: Vec<String> = s.graphemes(true).map(String::from).collect();
        let n = new.len();
        g.splice(self.cursor..self.cursor, new);
        self.text = g.concat();
        self.cursor += n;
    }

    fn delete_back(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let mut g: Vec<String> = self.graphemes().iter().map(|x| x.to_string()).collect();
        g.remove(self.cursor - 1);
        self.text = g.concat();
        self.cursor -= 1;
    }

    fn delete_forward(&mut self) {
        let mut g: Vec<String> = self.graphemes().iter().map(|x| x.to_string()).collect();
        if self.cursor < g.len() {
            g.remove(self.cursor);
            self.text = g.concat();
        }
    }

    /// The text cut into rows of at most `width` columns (a row break at each newline).
    fn rows(&self, width: usize) -> Vec<String> {
        let width = width.max(1);
        let mut rows = Vec::new();
        for src in self.text.split('\n') {
            let mut cur = String::new();
            let mut w = 0;
            for g in src.graphemes(true) {
                let gw = width_of(g);
                if w + gw > width && !cur.is_empty() {
                    rows.push(std::mem::take(&mut cur));
                    w = 0;
                }
                cur.push_str(g);
                w += gw;
            }
            rows.push(cur);
        }
        rows
    }

    fn text_height(&self, width: u16) -> u16 {
        (self.rows(width.saturating_sub(2) as usize).len() as u16).clamp(1, 8)
    }

    fn input_height(&self, width: u16) -> u16 {
        (self.text_height(width) + 1).min(9)
    }
}

impl BottomView for PromptView {
    fn desired_height(&self, width: u16) -> u16 {
        let extra = u16::from(self.context_label.is_some());
        1 + extra + self.input_height(width) + 3
    }

    fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.is_empty() {
            return;
        }
        let put = |buf: &mut Buffer, y: u16, l: Line<'_>| {
            tuikit::paint::put_line(buf, area.x, y, &l, area);
        };
        put(
            buf,
            area.y,
            Line::from(vec![gutter(), Span::from(self.title.clone()).bold()]),
        );
        let mut y = area.y + 1;
        if let Some(c) = &self.context_label {
            put(
                buf,
                y,
                Line::from(vec![gutter(), Span::from(c.clone()).cyan()]),
            );
            y += 1;
        }
        let input_h = self.input_height(area.width);
        for row in 0..input_h {
            put(buf, y + row, Line::from(gutter()));
        }
        // First input row stays blank, the text starts under it.
        let rows = self.rows(area.width.saturating_sub(2) as usize);
        for (i, r) in rows.iter().take(input_h as usize - 1).enumerate() {
            let yy = y + 1 + i as u16;
            tuikit::paint::put_str(
                buf,
                area.x + 2,
                yy,
                r,
                ratatui::style::Style::default(),
                area,
            );
        }
        if self.text.is_empty() {
            tuikit::paint::put_str(
                buf,
                area.x + 2,
                y + 1,
                &self.placeholder,
                ratatui::style::Style::default().dim(),
                area,
            );
        }
        let hint_y = y + input_h + 1;
        if hint_y < area.bottom() {
            put(
                buf,
                hint_y,
                Line::from(vec![
                    "Press ".into(),
                    "enter".dim(),
                    " to confirm or ".into(),
                    "esc".dim(),
                    " to go back".into(),
                ]),
            );
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> ViewResult {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => return ViewResult::Close,
            KeyCode::Char('c') if ctrl => return ViewResult::Close,
            KeyCode::Enter if key.modifiers == KeyModifiers::NONE => {
                let t = self.text.trim().to_string();
                if !t.is_empty() {
                    return ViewResult::CloseWith((self.submit)(t));
                }
            }
            KeyCode::Enter => self.insert("\n"),
            KeyCode::Backspace => self.delete_back(),
            KeyCode::Delete => self.delete_forward(),
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.graphemes().len()),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.graphemes().len(),
            KeyCode::Char('a') if ctrl => self.cursor = 0,
            KeyCode::Char('e') if ctrl => self.cursor = self.graphemes().len(),
            KeyCode::Char('u') if ctrl => {
                self.text.clear();
                self.cursor = 0;
            }
            KeyCode::Char(c)
                if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) && !c.is_control() =>
            {
                self.insert(&c.to_string());
            }
            _ => {}
        }
        ViewResult::Pending
    }

    fn handle_paste(&mut self, text: &str) {
        self.insert(&text.replace('\r', "\n"));
    }

    fn cursor(&self, area: Rect) -> Option<Position> {
        if area.height < 2 || area.width <= 2 {
            return None;
        }
        let width = area.width.saturating_sub(2) as usize;
        let before: String = self.graphemes()[..self.cursor.min(self.graphemes().len())].concat();
        let rows = PromptView {
            text: before,
            title: String::new(),
            placeholder: String::new(),
            context_label: None,
            cursor: 0,
            submit: self.submit.clone(),
        }
        .rows(width);
        let row = rows.len().saturating_sub(1) as u16;
        let col = rows.last().map_or(0, |r| width_of(r)) as u16;
        let top = 1 + u16::from(self.context_label.is_some()) + 1;
        Some(Position::new(area.x + 2 + col, area.y + top + row))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view() -> PromptView {
        PromptView::new(
            "Name thread",
            "Type a name and press Enter",
            None,
            Arc::new(AppAction::RenameSession),
        )
    }

    fn rows(v: &PromptView, w: u16) -> Vec<String> {
        let h = v.desired_height(w);
        let area = Rect::new(0, 0, w, h);
        let mut buf = Buffer::empty(area);
        v.render(area, &mut buf);
        (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    #[test]
    fn matches_the_capture_rows() {
        let v = view();
        assert_eq!(
            rows(&v, 120),
            [
                "▌ Name thread",
                "▌",
                "▌ Type a name and press Enter",
                "",
                "Press enter to confirm or esc to go back",
                "",
            ]
        );
    }

    #[test]
    fn typing_edits_at_the_cursor_and_enter_submits_trimmed_text() {
        let mut v = view();
        for c in " my-thrd".chars() {
            v.handle_key(key(KeyCode::Char(c)));
        }
        v.handle_key(key(KeyCode::Left));
        v.handle_key(key(KeyCode::Char('e')));
        v.handle_key(key(KeyCode::Char('a')));
        assert_eq!(rows(&v, 40)[2], "▌  my-thread");
        assert_eq!(
            v.handle_key(key(KeyCode::Enter)),
            ViewResult::CloseWith(AppAction::RenameSession("my-thread".into()))
        );
    }

    #[test]
    fn empty_enter_stays_and_escape_closes() {
        let mut v = view();
        assert_eq!(v.handle_key(key(KeyCode::Enter)), ViewResult::Pending);
        assert_eq!(v.handle_key(key(KeyCode::Esc)), ViewResult::Close);
    }

    #[test]
    fn cursor_follows_the_text() {
        let mut v = view();
        v.handle_key(key(KeyCode::Char('a')));
        v.handle_key(key(KeyCode::Char('b')));
        let p = v.cursor(Rect::new(0, 10, 40, 6)).unwrap();
        assert_eq!((p.x, p.y), (4, 12));
    }
}
