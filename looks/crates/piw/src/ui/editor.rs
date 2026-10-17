// OWNER: editor (input box: wrapping, paste markers, history, cursor)
//! The editor box: two full-width rules with the text between them, no side borders, no prompt
//! glyph. The cursor is a reverse-video cell. The model (text, undo, kill ring, history) is
//! `tuikit::editor::Editor`; this file adds Pi's paste markers and draws the box. Spec 5.

use std::cell::Cell;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use tuikit::editor::{Editor, KeyOutcome, VisualRow};
use unicode_segmentation::UnicodeSegmentation;

use super::loader::{self, Loader};
use super::{span, Cx, Lines};
use crate::theme::Tok;

/// A paste kept aside while the editor shows a marker for it.
#[derive(Clone, Debug)]
struct Paste {
    marker: String,
    text: String,
}

#[derive(Debug)]
pub struct EditorState {
    pub ed: Editor,
    pastes: Vec<Paste>,
    paste_seq: u32,
    scroll: Cell<usize>,
    /// `editorPaddingX`: columns kept clear on each side, 0 to 3.
    pub pad: u16,
}

pub enum EditKey {
    /// Not an editing key; the caller decides.
    Ignored,
    Handled,
    /// `up` on the first row: the caller walks history.
    AtTop,
    AtBottom,
}

/// Tabs become four spaces, `\r\n` and `\r` become `\n`, other control characters go.
pub fn clean_paste(s: &str) -> String {
    let s = s.replace("\r\n", "\n").replace('\r', "\n");
    s.chars()
        .flat_map(|c| match c {
            '\t' => vec![' '; 4],
            '\n' => vec!['\n'],
            c if c.is_control() => vec![],
            c => vec![c],
        })
        .collect()
}

impl Default for EditorState {
    fn default() -> Self {
        let mut ed = Editor::new();
        ed.set_pi_wrap(true);
        ed.set_pi_keys(true);
        EditorState {
            ed,
            pastes: Vec::new(),
            paste_seq: 0,
            scroll: Cell::new(0),
            pad: 0,
        }
    }
}

impl EditorState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn text(&self) -> &str {
        self.ed.text()
    }

    pub fn is_empty(&self) -> bool {
        self.ed.text().is_empty()
    }

    pub fn clear(&mut self) {
        self.ed.clear();
        self.pastes.clear();
        self.paste_seq = 0;
        self.sync_markers();
    }

    pub fn set_text(&mut self, s: &str) {
        self.pastes.clear();
        self.paste_seq = 0;
        self.ed.set_text(s);
        self.sync_markers();
    }

    /// Word motion and kills treat a paste marker as one unit, so the model has to know them.
    fn sync_markers(&mut self) {
        self.ed
            .set_atomic_markers(self.pastes.iter().map(|p| p.marker.clone()).collect());
    }

    /// The text with every paste marker replaced by what was pasted.
    pub fn expanded(&self) -> String {
        let mut t = self.ed.text().to_string();
        for p in &self.pastes {
            t = t.replacen(&p.marker, &p.text, 1);
        }
        t
    }

    pub fn insert_str(&mut self, s: &str) {
        self.ed.paste(&clean_paste(s));
    }

    /// Bracketed paste. Long ones are stored aside and shown as `[paste #1 +30 lines]`.
    pub fn paste(&mut self, raw: &str) {
        let mut text = clean_paste(raw);
        let lines = text.split('\n').count();
        let chars = text.chars().count();
        if lines > 10 || chars > 1000 {
            self.paste_seq += 1;
            let marker = if lines > 10 {
                format!("[paste #{} +{lines} lines]", self.paste_seq)
            } else {
                format!("[paste #{} {chars} chars]", self.paste_seq)
            };
            self.pastes.push(Paste {
                marker: marker.clone(),
                text,
            });
            self.sync_markers();
            self.ed.paste(&marker);
            return;
        }
        // a path pasted right after a word gets a space in front
        let before = self.ed.text()[..self.ed.cursor()].chars().next_back();
        if text.starts_with(['/', '~', '.'])
            && before.is_some_and(|c| c.is_alphanumeric() || c == '_')
        {
            text.insert(0, ' ');
        }
        self.ed.paste(&text);
    }

    /// Marker ending exactly at the cursor, if any.
    fn marker_before(&self) -> Option<(usize, usize)> {
        let c = self.ed.cursor();
        let t = self.ed.text();
        self.pastes
            .iter()
            .find_map(|p| t[..c].ends_with(&p.marker).then(|| (c - p.marker.len(), c)))
    }

    fn marker_after(&self) -> Option<(usize, usize)> {
        let c = self.ed.cursor();
        let t = self.ed.text();
        self.pastes.iter().find_map(|p| {
            t[c..]
                .starts_with(&p.marker)
                .then(|| (c, c + p.marker.len()))
        })
    }

    fn cut(&mut self, a: usize, b: usize) {
        let t = self.ed.text();
        let new = format!("{}{}", &t[..a], &t[b..]);
        let removed = t[a..b].to_string();
        self.pastes.retain(|p| p.marker != removed);
        self.sync_markers();
        self.ed.set_text(&new);
        self.ed.set_cursor(a);
    }

    /// Apply an editing key. Markers are atomic: the cursor steps over them and backspace or
    /// delete removes one whole.
    pub fn key(&mut self, key: KeyEvent) -> EditKey {
        let plain = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        if plain && !self.pastes.is_empty() {
            match key.code {
                KeyCode::Backspace => {
                    if let Some((a, b)) = self.marker_before() {
                        self.cut(a, b);
                        return EditKey::Handled;
                    }
                }
                KeyCode::Delete => {
                    if let Some((a, b)) = self.marker_after() {
                        self.cut(a, b);
                        return EditKey::Handled;
                    }
                }
                KeyCode::Left => {
                    if let Some((a, _)) = self.marker_before() {
                        self.ed.set_cursor(a);
                        return EditKey::Handled;
                    }
                }
                KeyCode::Right => {
                    if let Some((_, b)) = self.marker_after() {
                        self.ed.set_cursor(b);
                        return EditKey::Handled;
                    }
                }
                _ => {}
            }
        }
        match self.ed.apply_key(key) {
            KeyOutcome::Ignored => EditKey::Ignored,
            KeyOutcome::Handled => EditKey::Handled,
            KeyOutcome::AtTop => EditKey::AtTop,
            KeyOutcome::AtBottom => EditKey::AtBottom,
        }
    }

    /// Columns each side that padding actually takes at this width.
    fn effective_pad(&self, screen_w: u16) -> u16 {
        self.pad.min(screen_w.saturating_sub(1) / 2)
    }

    /// Wrap width: the content width, less one column kept for the cursor when there is no
    /// padding (with padding the cursor may overflow into it).
    fn wrap_width(&self, screen_w: u16) -> usize {
        let pad = self.effective_pad(screen_w);
        let content = screen_w.saturating_sub(2 * pad).max(1);
        content.saturating_sub(u16::from(pad == 0)).max(1) as usize
    }

    pub fn set_width(&mut self, screen_w: u16) {
        self.ed.set_wrap_width(self.wrap_width(screen_w) as u16);
    }

    pub fn is_bash(&self) -> bool {
        self.ed.text().trim_start().starts_with('!')
    }

    /// Visual rows at the wrap width (`W - 1` without padding; one column is kept for the cursor).
    pub fn layout(&self, screen_w: u16) -> Vec<VisualRow> {
        self.ed.layout(self.wrap_width(screen_w))
    }
}

/// Draw the box. `loader` replaces the top rule's dashes while the agent works.
pub fn render(
    st: &EditorState,
    cx: &Cx,
    max_rows: usize,
    loader: Option<&Loader>,
    bash: bool,
    thinking: Tok,
) -> Lines {
    let w = cx.width as usize;
    let tok = if bash { Tok::BashMode } else { thinking };
    let rule = cx.th().fg(tok);
    let text = st.ed.text();
    let rows = st.layout(cx.width);
    let cursor = st.ed.cursor();
    let cur_row = rows
        .iter()
        .position(|r| {
            cursor >= r.start && (cursor < r.next || (r.last_in_line && cursor <= r.next))
        })
        .unwrap_or(rows.len().saturating_sub(1));
    let mut scroll = st.scroll.get();
    if cur_row < scroll {
        scroll = cur_row;
    } else if cur_row >= scroll + max_rows {
        scroll = cur_row + 1 - max_rows;
    }
    scroll = scroll.min(rows.len().saturating_sub(max_rows));
    st.scroll.set(scroll);
    let visible = &rows[scroll..(scroll + max_rows).min(rows.len())];
    let below = rows.len() - (scroll + visible.len());

    let pad = " ".repeat(st.effective_pad(cx.width) as usize);
    let mut out = vec![loader::top_border(w, rule, loader, scroll)];
    for (i, r) in visible.iter().enumerate() {
        let is_cursor_row = scroll + i == cur_row;
        let content = text[r.start..r.end].to_string();
        if !is_cursor_row {
            out.push(Line::from(vec![Span::raw(pad.clone()), Span::raw(content)]));
            continue;
        }
        let at = cursor.clamp(r.start, r.end) - r.start;
        let before = content[..at].to_string();
        let after = &content[at..];
        let rev = Style::default().add_modifier(Modifier::REVERSED);
        let mut spans: Vec<Span<'static>> = vec![Span::raw(pad.clone()), Span::raw(before)];
        match after.graphemes(true).next() {
            Some(g) => {
                spans.push(span(g.to_string(), rev));
                spans.push(Span::raw(after[g.len()..].to_string()));
            }
            None => spans.push(span(" ", rev)),
        }
        out.push(Line::from(spans));
    }
    out.push(loader::bottom_border(w, rule, below));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::PiTheme;
    use std::time::Duration;

    fn cx(w: u16) -> Cx {
        Cx {
            theme: PiTheme::dark(),
            width: w,
            expanded: false,
            hide_thinking: false,
            out_pad: 1,
            cwd: String::new(),
            home: String::new(),
            clock: Duration::ZERO,
            version: "1.0.3".into(),
        }
    }

    fn text(l: &Line) -> String {
        l.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn empty_editor_is_one_cursor_cell_between_rules() {
        let st = EditorState::new();
        let rows = render(&st, &cx(20), 10, None, false, Tok::ThinkingMedium);
        assert_eq!(rows.len(), 3);
        assert_eq!(text(&rows[0]), "─".repeat(20));
        assert_eq!(text(&rows[1]), " ");
        assert!(rows[1]
            .spans
            .iter()
            .any(|s| s.style.add_modifier.contains(Modifier::REVERSED)));
    }

    #[test]
    fn long_paste_is_one_marker_and_expands_on_submit() {
        let mut st = EditorState::new();
        let big: String = (1..=30).map(|i| format!("l{i}\n")).collect();
        st.paste(&big);
        assert_eq!(st.text(), "[paste #1 +31 lines]");
        assert!(st.expanded().starts_with("l1\nl2"));
        st.key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(st.text(), "");
    }

    #[test]
    fn character_paste_marker() {
        let mut st = EditorState::new();
        st.paste(&"x".repeat(1500));
        assert_eq!(st.text(), "[paste #1 1500 chars]");
    }

    #[test]
    fn grows_then_scrolls_with_labels() {
        let mut st = EditorState::new();
        for i in 1..=13 {
            st.ed.paste(&format!("line {i}"));
            st.ed.insert_newline();
        }
        let rows = render(&st, &cx(40), 10, None, false, Tok::ThinkingMedium);
        assert_eq!(rows.len(), 12);
        assert!(text(&rows[0]).contains(" ↑ "), "{}", text(&rows[0]));
    }
}
