// OWNER: input
//! The composer: bar, raised background, growth to 8 rows, paste and image chips, history
//! recall and the completion popups. The popup drawing lives in `popup`, the dock in `dock`.

use agent_core::ImageData;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use tuikit::editor::{Editor, EditorStyle, KeyOutcome};
use tuikit::paint::{fill, put_str};
use tuikit::width::{display_width, truncate};

use crate::clip::{self, Image};
use crate::commands::Cmd;
use crate::palette::{Glyphs, Palette};

pub use crate::ui::dock::{dock_rows, DOCK_MAX};
pub use crate::ui::popup::{draw_popup, PItem, PopKind, Popup, PopupPlace};

pub const MAX_ROWS: u16 = 8;
/// Pastes of this many lines or characters become a chip.
const CHIP_LINES: usize = 8;
const CHIP_CHARS: usize = 1000;
/// Chips kept for recalling a sent message from history, by total payload bytes.
const ARCHIVE_BYTES: usize = 48 * 1024 * 1024;
/// Most files a `@` popup scores.
const POPUP_ROWS: usize = 50;

#[derive(Clone, Debug)]
pub struct Chip {
    pub label: String,
    /// Pasted text, put back in place of the label when the message is sent.
    pub text: String,
    pub image: Option<Image>,
}

impl Chip {
    fn bytes(&self) -> usize {
        self.text.len() + self.image.as_ref().map_or(0, |i| i.bytes.len())
    }
}

pub struct Composer {
    pub ed: Editor,
    /// Every chip made in this session, oldest first. Labels are unique, so a message recalled
    /// from history finds its pasted text again; only labels present in the text are used.
    pub chips: Vec<Chip>,
    pub popup: Option<Popup>,
    /// Faint argument hint after a complete `/command `.
    pub ghost: Option<String>,
    /// Escape-closed popups stay closed until the token changes.
    dismissed: Option<String>,
    /// The highlighted row of the popup was chosen with the arrow keys, so Enter completes it
    /// instead of sending what is typed.
    picked: bool,
    /// The file index is too big to score on the UI thread: `update_popup` leaves the query in
    /// `want_files` for the app to hand to the search worker, and the rows arrive later through
    /// [`apply_hits`](Self::apply_hits).
    pub remote_files: bool,
    pub want_files: Option<String>,
    /// A query is out and the rows on screen are for an older one, so Tab and Enter wait.
    file_pending: bool,
}

/// Pasted text with newlines normalised and every escape sequence removed whole (not just its
/// ESC byte), so a small paste and a chip carry the same text. Tabs stay.
fn clean_paste(s: &str) -> String {
    let s = s.replace("\r\n", "\n").replace('\r', "\n");
    s.split('\t')
        .map(tuikit::width::plain_text)
        .collect::<Vec<_>>()
        .join("\t")
}

/// The `@word` or `@"quoted word"` ending at the caret: where its `@` is and what to search.
/// A quoted token may hold spaces; a plain one ends at whitespace.
fn file_token(text: &str, cur: usize) -> Option<(usize, String)> {
    let before = &text[..cur];
    let at_ok = |s: usize| s == 0 || text[..s].ends_with(char::is_whitespace);
    if let Some(s) = before.rfind("@\"").filter(|&s| at_ok(s)) {
        let q = &before[s + 2..];
        if !q.contains('"') {
            return Some((s, q.to_string()));
        }
    }
    let s = before
        .rfind(|c: char| c.is_whitespace() || c == '@')
        .filter(|i| text[*i..].starts_with('@'))?;
    let q = &text[s + 1..cur];
    (at_ok(s) && !q.contains(char::is_whitespace)).then(|| (s, q.to_string()))
}

impl Default for Composer {
    fn default() -> Self {
        Self::new()
    }
}

/// What the app should do after a key reached the composer.
#[derive(Debug, PartialEq, Eq)]
pub enum Out {
    None,
    Send,
    Newline,
    /// Up on the first row with nothing to walk back to.
    HistoryEdge,
}

impl Composer {
    pub fn new() -> Composer {
        Composer {
            ed: Editor::new(),
            chips: Vec::new(),
            popup: None,
            ghost: None,
            dismissed: None,
            picked: false,
            remote_files: false,
            want_files: None,
            file_pending: false,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.ed.is_empty()
    }

    pub fn text(&self) -> &str {
        self.ed.text()
    }

    /// Replace the text (history recall, a restored draft, the editor coming back). Chips
    /// whose label is still in the text keep their payload.
    pub fn set_text(&mut self, s: &str) {
        self.ed.set_text(s);
        self.popup = None;
        self.ghost = None;
    }

    pub fn clear(&mut self) {
        self.set_text("");
    }

    /// A label not used by any chip yet: `[paste 40 lines]`, then `[paste 40 lines #2]`.
    fn unique_label(&self, base: &str) -> String {
        let free = |l: &str| self.chips.iter().all(|c| c.label != l);
        if free(base) {
            return base.to_string();
        }
        let stem = base.trim_end_matches(']');
        (2..)
            .map(|n| format!("{stem} #{n}]"))
            .find(|l| free(l))
            .unwrap_or_else(|| base.to_string())
    }

    fn add_chip(&mut self, label: String, text: String, image: Option<Image>) {
        self.ed.paste(&label);
        self.chips.push(Chip { label, text, image });
        // Old payloads go first once the archive is large.
        let mut total: usize = self.chips.iter().map(Chip::bytes).sum();
        while total > ARCHIVE_BYTES && self.chips.len() > 1 {
            total -= self.chips.remove(0).bytes();
        }
    }

    /// Insert pasted text: small pastes inline, large ones as a chip, a path to an image file
    /// as an image chip.
    pub fn paste(&mut self, s: &str, cwd: &std::path::Path) {
        let s = clean_paste(s);
        // A copied line usually carries its newline; it is not part of what you meant to paste.
        // Several lines are sent exactly as pasted.
        let one = s
            .strip_suffix('\n')
            .filter(|t| !t.is_empty() && !t.contains('\n'));
        let s = one.unwrap_or(s.as_str());
        if let Some(p) = clip::image_path(s, cwd) {
            if let Ok(img) = clip::from_file(&p) {
                self.paste_image(img);
                return;
            }
        }
        let lines = s.lines().count();
        if lines >= CHIP_LINES || s.chars().count() >= CHIP_CHARS {
            let base = if lines > 1 {
                format!("[paste {lines} lines]")
            } else {
                format!("[paste {} chars]", s.chars().count())
            };
            let label = self.unique_label(&base);
            self.add_chip(label, s.to_string(), None);
        } else {
            self.ed.paste(s);
        }
        self.popup = None;
    }

    /// Put an image chip at the caret.
    pub fn paste_image(&mut self, img: Image) {
        let label = self.unique_label(&img.label());
        self.add_chip(label, String::new(), Some(img));
        self.popup = None;
    }

    /// Chips whose label is in `text`, in the order they appear.
    fn chips_in(&self, text: &str) -> Vec<&Chip> {
        let mut v: Vec<(usize, &Chip)> = self
            .chips
            .iter()
            .filter_map(|c| text.find(&c.label).map(|i| (i, c)))
            .collect();
        v.sort_by_key(|(i, _)| *i);
        v.into_iter().map(|(_, c)| c).collect()
    }

    fn live_chips(&self) -> Vec<&Chip> {
        self.chips_in(self.ed.text())
    }

    /// `text` with pasted text in place of its chips. An image chip stays as its label, so the
    /// sentence still reads, and the image travels beside it.
    pub fn expand_text(&self, text: &str) -> String {
        let mut out = text.to_string();
        for c in self.chips_in(text) {
            if c.image.is_none() {
                out = out.replacen(&c.label, &c.text, 1);
            }
        }
        out
    }

    /// The text to send.
    pub fn expanded(&self) -> String {
        self.expand_text(self.ed.text())
    }

    /// Images whose chip is in `text`.
    pub fn images_in(&self, text: &str) -> Vec<ImageData> {
        self.chips_in(text)
            .into_iter()
            .filter_map(|c| c.image.as_ref().map(Image::to_data))
            .collect()
    }

    /// Images whose chip is still in the composer.
    pub fn images(&self) -> Vec<ImageData> {
        self.images_in(self.ed.text())
    }

    /// Take the message: expanded text and the shown text (chips kept as chips).
    pub fn take(&mut self) -> (String, String) {
        let full = self.expanded();
        let shown = self.ed.text().to_string();
        self.ed.submit();
        self.popup = None;
        self.ghost = None;
        (full, shown)
    }

    /// Backspace removes a whole chip when the caret sits right after one.
    fn backspace_chip(&mut self) -> bool {
        let cur = self.ed.cursor();
        let text = self.ed.text();
        let Some(label) = self
            .chips
            .iter()
            .rev()
            .find(|c| text[..cur].ends_with(&c.label))
            .map(|c| c.label.clone())
        else {
            return false;
        };
        // One edit at a time so undo brings the chip back as a unit.
        for _ in 0..label.chars().count() {
            self.ed.backspace();
        }
        true
    }

    pub fn on_key(&mut self, key: KeyEvent, cmds: &[Cmd], files: &[String]) -> Out {
        let m = key.modifiers;
        let plain = !m.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        let ctrl = m.contains(KeyModifiers::CONTROL);
        // Popup navigation first.
        if let Some(pop) = self.popup.as_mut() {
            let n = pop.items.len().max(1);
            match key.code {
                KeyCode::Up if plain => {
                    pop.sel = (pop.sel + n - 1) % n;
                    self.picked = true;
                    return Out::None;
                }
                KeyCode::Down if plain => {
                    pop.sel = (pop.sel + 1) % n;
                    self.picked = true;
                    return Out::None;
                }
                KeyCode::Char('p') if ctrl => {
                    pop.sel = (pop.sel + n - 1) % n;
                    self.picked = true;
                    return Out::None;
                }
                KeyCode::Char('n') if ctrl => {
                    pop.sel = (pop.sel + 1) % n;
                    self.picked = true;
                    return Out::None;
                }
                KeyCode::Tab => {
                    self.picked = false;
                    self.accept();
                    self.update_popup(cmds, files);
                    return Out::None;
                }
                KeyCode::Esc => {
                    let tok = self.ed.text()[pop.token.clone()].to_string();
                    self.dismissed = Some(tok);
                    self.popup = None;
                    return Out::None;
                }
                KeyCode::Enter
                    if plain && !m.contains(KeyModifiers::SHIFT) && pop.kind == PopKind::Slash =>
                {
                    // Enter completes a row you moved to with the arrow keys, or the command the
                    // typed word is the start of (`/rai` is `/rail`); a second Enter then sends
                    // it. A word that only fuzzy-matches (`/cost` under `/design-consent`) is
                    // sent as typed, so it can reach claude. Tab always completes.
                    let typed = self.ed.text()[pop.token.clone()].to_string();
                    let chosen = pop
                        .items
                        .get(pop.sel)
                        .map(|i| i.insert.clone())
                        .unwrap_or_default();
                    let same = typed.trim_end() == chosen.trim_end();
                    let starts = chosen.starts_with(typed.trim_end());
                    if (self.picked || starts) && !chosen.is_empty() && !same {
                        self.picked = false;
                        self.accept();
                        self.update_popup(cmds, files);
                        return Out::None;
                    }
                }
                KeyCode::Enter
                    if plain && !m.contains(KeyModifiers::SHIFT) && pop.kind == PopKind::File =>
                {
                    self.accept();
                    self.update_popup(cmds, files);
                    return Out::None;
                }
                _ => {}
            }
        }
        self.picked = false;
        let out = match key.code {
            KeyCode::Enter if m.contains(KeyModifiers::SHIFT) || m.contains(KeyModifiers::ALT) => {
                self.ed.insert_newline();
                Out::Newline
            }
            KeyCode::Enter => {
                // `\` then enter is a newline, for terminals that cannot report shift+enter.
                let t = self.ed.text();
                if t[..self.ed.cursor()].ends_with('\\') {
                    self.ed.backspace();
                    self.ed.insert_newline();
                    Out::Newline
                } else {
                    return Out::Send;
                }
            }
            KeyCode::Char('j') if ctrl => {
                self.ed.insert_newline();
                Out::Newline
            }
            KeyCode::Backspace if plain && self.backspace_chip() => Out::None,
            _ => match self.ed.apply_key(key) {
                KeyOutcome::AtTop => {
                    // A draft that no entry starts with still walks, through every entry, and
                    // down brings the draft back.
                    if !self.ed.history_prev() && !self.ed.history_prev_any() {
                        Out::HistoryEdge
                    } else {
                        Out::None
                    }
                }
                KeyOutcome::AtBottom => {
                    self.ed.history_next();
                    Out::None
                }
                _ => Out::None,
            },
        };
        self.update_popup(cmds, files);
        out
    }

    /// Complete the selected popup item into the text.
    fn accept(&mut self) {
        if self.file_pending && self.popup.as_ref().is_some_and(|p| p.kind == PopKind::File) {
            // The rows are for an older query; completing one would insert the wrong path.
            return;
        }
        let Some(pop) = self.popup.take() else { return };
        let Some(item) = pop.items.get(pop.sel) else {
            return;
        };
        let mut t = self.ed.text().to_string();
        let quote = item.insert.contains(char::is_whitespace);
        let ins = match pop.kind {
            // A directory keeps the token open so you can keep typing inside it. A name with a
            // space is quoted, which is how claude reads it as one path.
            PopKind::File if item.insert.ends_with('/') && quote => format!("@\"{}", item.insert),
            PopKind::File if item.insert.ends_with('/') => format!("@{}", item.insert),
            PopKind::File if quote => format!("@\"{}\" ", item.insert),
            PopKind::File => format!("@{} ", item.insert),
            PopKind::Slash => format!("{} ", item.insert),
        };
        t.replace_range(pop.token.clone(), &ins);
        let pos = pop.token.start + ins.len();
        self.ed.set_text(&t);
        self.ed.set_cursor(pos);
    }

    /// Recompute the popup from the text and caret.
    pub fn update_popup(&mut self, cmds: &[Cmd], files: &[String]) {
        let text = self.ed.text();
        let cur = self.ed.cursor();
        self.ghost = None;
        // Slash: only at column 0 of the text, while still inside the first word.
        if text.starts_with('/') && !text[..cur.max(1)].contains('\n') {
            let end = text.find(char::is_whitespace).unwrap_or(text.len());
            if cur > end {
                // Past the command name: a faint hint for its argument, until you type one.
                let name = &text[1..end];
                if cur == text.len() && text[end..].trim().is_empty() {
                    self.ghost = cmds
                        .iter()
                        .find(|c| c.name == name)
                        .map(|c| c.hint.clone())
                        .filter(|h| !h.is_empty());
                }
            } else if !text[..cur].contains(char::is_whitespace) {
                let q = &text[1..end];
                if self.dismissed.as_deref() == Some(&text[..end]) {
                    self.popup = None;
                    return;
                }
                let items = crate::commands::match_items(cmds, q);
                let keep = self
                    .popup
                    .as_ref()
                    .filter(|p| p.kind == PopKind::Slash)
                    .map_or(0, |p| p.sel);
                self.popup = (!items.is_empty()).then(|| Popup {
                    kind: PopKind::Slash,
                    sel: keep.min(items.len() - 1),
                    items,
                    token: 0..end,
                });
                return;
            }
        }
        // File: the `@word` or `@"quoted word"` the caret is in.
        if let Some((s, q)) = file_token(text, cur) {
            if self.dismissed.as_deref() == Some(&text[s..cur]) {
                self.popup = None;
                return;
            }
            let end = cur;
            if self.remote_files {
                self.want_files = Some(q.to_string());
                self.file_pending = true;
                // Keep the old rows on screen, aimed at the token as it is now.
                if let Some(p) = self.popup.as_mut().filter(|p| p.kind == PopKind::File) {
                    p.token = s..end;
                }
                return;
            }
            let items = crate::ui::popup::file_items(files, &q, POPUP_ROWS);
            let keep = self
                .popup
                .as_ref()
                .filter(|p| p.kind == PopKind::File)
                .map_or(0, |p| p.sel);
            self.popup = (!items.is_empty()).then(|| Popup {
                kind: PopKind::File,
                sel: keep.min(items.len() - 1),
                items,
                token: s..end,
            });
            return;
        }
        self.want_files = None;
        self.file_pending = false;
        self.dismissed = None;
        self.popup = None;
    }

    /// Rows from the search worker for `q`. Dropped when the text has moved on to another query
    /// (a newer answer is on its way).
    pub fn apply_hits(&mut self, q: &str, items: Vec<PItem>) {
        let text = self.ed.text();
        let Some((s, now_q)) = file_token(text, self.ed.cursor()) else {
            return;
        };
        if now_q != q {
            return;
        }
        self.file_pending = false;
        let keep = self
            .popup
            .as_ref()
            .filter(|p| p.kind == PopKind::File)
            .map_or(0, |p| p.sel);
        self.popup = (!items.is_empty()).then(|| Popup {
            kind: PopKind::File,
            sel: keep.min(items.len() - 1),
            items,
            token: s..self.ed.cursor(),
        });
    }

    /// Rows the composer wants at inner width `w`.
    pub fn content_rows(&self, w: u16) -> u16 {
        self.ed.needed_height(w).clamp(1, MAX_ROWS)
    }

    /// Draw the bar, background and text. Returns the caret cell. `hint` is a faint right
    /// aligned note on the first row, used for the busy state.
    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &mut self,
        buf: &mut Buffer,
        rect: Rect,
        pad: bool,
        focused: bool,
        placeholder: &str,
        hint: &str,
        p: &Palette,
        g: &Glyphs,
    ) -> Option<(u16, u16)> {
        fill(buf, rect, Style::new().bg(p.raised));
        let bar = Style::new()
            .fg(if focused { p.user } else { p.line })
            .bg(p.raised);
        for y in rect.top()..rect.bottom() {
            put_str(buf, rect.x, y, g.bar, bar, rect);
        }
        let top = u16::from(pad);
        let text_area = Rect::new(
            rect.x + 2,
            rect.y + top,
            rect.width.saturating_sub(3),
            rect.height.saturating_sub(top * 2),
        );
        let style = EditorStyle {
            text: Style::new().fg(p.text).bg(p.raised),
            placeholder: Some((
                placeholder.to_string(),
                Style::new().fg(p.faint).bg(p.raised),
            )),
        };
        let info = self.ed.render(text_area, buf, &style);
        // Chips look like chips inside the editor too.
        let chip_style = Style::new().fg(p.dim).bg(p.line);
        for c in self.live_chips() {
            highlight_label(buf, text_area, &c.label, chip_style);
        }
        if let (Some(g), Some((cx, cy))) = (&self.ghost, info.cursor) {
            put_str(
                buf,
                cx,
                cy,
                g,
                Style::new().fg(p.faint).bg(p.raised),
                text_area,
            );
        }
        // The hint only goes where the first row has the room, never over the text.
        if !hint.is_empty() && !self.ed.is_empty() && text_area.width > 0 {
            let first_row_end = (text_area.left()..text_area.right())
                .rev()
                .find(|x| {
                    buf.cell((*x, text_area.y))
                        .is_some_and(|c| c.symbol() != " ")
                })
                .map_or(text_area.x, |x| x + 1);
            let w = display_width(hint) as u16;
            if text_area.right() >= first_row_end + w + 3 {
                put_str(
                    buf,
                    text_area.right() - w,
                    text_area.y,
                    hint,
                    Style::new().fg(p.faint).bg(p.raised),
                    text_area,
                );
            }
        }
        info.cursor
    }
}

/// What the composer says about the state it is in: `(placeholder, hint)`. The placeholder
/// shows while it is empty; the hint is a short note on the first row once there is text.
pub fn placeholder(busy: bool, queued: usize, width: u16) -> (String, String) {
    if !busy {
        let ph = if width >= 56 {
            "Message claude  /  commands  @  files"
        } else {
            "Message claude"
        };
        return (ph.to_string(), String::new());
    }
    let long = if queued > 0 {
        "enter queues  up takes the last back  ctrl+s steers"
    } else {
        "enter queues  ctrl+s steers  esc esc interrupts"
    };
    let short = "enter queues  ctrl+s steers";
    let ph = if width >= 60 { long } else { short };
    (
        truncate(ph, width.saturating_sub(4) as usize),
        short.to_string(),
    )
}

/// Re-style the first occurrence of `label` on screen (single row only).
fn highlight_label(buf: &mut Buffer, area: Rect, label: &str, st: Style) {
    for y in area.top()..area.bottom() {
        let mut row = String::new();
        let mut xs = Vec::new();
        for x in area.left()..area.right() {
            if let Some(c) = buf.cell((x, y)) {
                xs.push((x, row.len()));
                row.push_str(c.symbol());
            }
        }
        if let Some(i) = row.find(label) {
            let n = label.chars().count();
            let first = xs.iter().position(|(_, b)| *b == i).unwrap_or(0);
            for k in 0..n {
                if let Some((x, _)) = xs.get(first + k) {
                    if let Some(c) = buf.cell_mut((*x, y)) {
                        c.set_style(st);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::registry;
    use std::path::Path;

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    fn type_str(c: &mut Composer, s: &str, cmds: &[Cmd]) {
        for ch in s.chars() {
            c.on_key(key(KeyCode::Char(ch)), cmds, &[]);
        }
    }

    fn paste(c: &mut Composer, s: &str) {
        c.paste(s, Path::new("/"));
    }

    fn lines(n: usize) -> String {
        (0..n).map(|i| format!("line {i}\n")).collect()
    }

    #[test]
    fn large_paste_is_one_chip_that_expands_on_send() {
        let mut c = Composer::new();
        paste(&mut c, &lines(40));
        assert_eq!(c.text(), "[paste 40 lines]");
        let (full, shown) = c.take();
        assert_eq!(full.lines().count(), 40);
        assert_eq!(shown, "[paste 40 lines]");
        assert!(c.is_empty());
    }

    #[test]
    fn small_paste_is_inline_and_backspace_removes_a_chip_whole() {
        let mut c = Composer::new();
        paste(&mut c, "a\nb");
        assert_eq!(c.text(), "a\nb");
        c.clear();
        paste(&mut c, &lines(9));
        c.on_key(key(KeyCode::Backspace), &[], &[]);
        assert!(c.is_empty());
    }

    #[test]
    fn a_copied_line_loses_its_trailing_newline() {
        let mut c = Composer::new();
        paste(&mut c, "cargo test\n");
        assert_eq!(c.text(), "cargo test");
    }

    #[test]
    fn two_pastes_of_the_same_size_stay_apart() {
        let mut c = Composer::new();
        let a: String = (0..10).map(|i| format!("a{i}\n")).collect();
        let b: String = (0..10).map(|i| format!("b{i}\n")).collect();
        paste(&mut c, &a);
        type_str(&mut c, " and ", &[]);
        paste(&mut c, &b);
        assert_eq!(c.text(), "[paste 10 lines] and [paste 10 lines #2]");
        let full = c.expanded();
        assert!(
            full.starts_with("a0\n") && full.contains("a9\n and b0\n"),
            "{full:?}"
        );
        assert!(full.trim_end().ends_with("b9"));
    }

    #[test]
    fn undo_takes_a_paste_back_out_and_the_chip_can_be_pasted_again() {
        let mut c = Composer::new();
        paste(&mut c, &lines(12));
        let undo = KeyEvent::new(KeyCode::Char('_'), KeyModifiers::CONTROL);
        c.on_key(undo, &[], &[]);
        assert!(c.is_empty(), "{:?}", c.text());
        // The same size again must not pick up the stale payload.
        let other: String = (0..12).map(|i| format!("other {i}\n")).collect();
        paste(&mut c, &other);
        assert!(c.expanded().starts_with("other 0"), "{:?}", c.expanded());
    }

    #[test]
    fn a_message_recalled_from_history_still_expands() {
        let mut c = Composer::new();
        paste(&mut c, &lines(10));
        let (full, _) = c.take();
        assert!(full.contains("line 9"));
        // Up on an empty composer recalls what was sent, chip label and all.
        c.on_key(key(KeyCode::Up), &[], &[]);
        assert_eq!(c.text(), "[paste 10 lines]");
        assert!(c.expanded().contains("line 9"));
    }

    #[test]
    fn an_image_chip_travels_beside_the_text() {
        let mut c = Composer::new();
        type_str(&mut c, "what is ", &[]);
        let mut bytes = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        bytes.extend(1280u32.to_be_bytes());
        bytes.extend(720u32.to_be_bytes());
        bytes.extend([8, 2, 0, 0, 0]);
        c.paste_image(clip::from_bytes(bytes).unwrap());
        assert_eq!(c.text(), "what is [image 1280x720]");
        assert_eq!(c.expanded(), "what is [image 1280x720]");
        let imgs = c.images();
        assert_eq!(imgs.len(), 1);
        assert_eq!(imgs[0].media_type, "image/png");
        // Deleting the chip drops the image.
        c.on_key(key(KeyCode::Backspace), &[], &[]);
        assert!(c.images().is_empty());
    }

    #[test]
    fn backslash_enter_is_a_newline_and_plain_enter_sends() {
        let mut c = Composer::new();
        type_str(&mut c, "a\\", &[]);
        assert_eq!(c.on_key(key(KeyCode::Enter), &[], &[]), Out::Newline);
        assert_eq!(c.text(), "a\n");
        type_str(&mut c, "b", &[]);
        assert_eq!(c.on_key(key(KeyCode::Enter), &[], &[]), Out::Send);
    }

    #[test]
    fn shift_alt_and_ctrl_j_all_make_a_newline() {
        for m in [
            KeyModifiers::SHIFT,
            KeyModifiers::ALT,
            KeyModifiers::CONTROL,
        ] {
            let mut c = Composer::new();
            type_str(&mut c, "a", &[]);
            let code = if m == KeyModifiers::CONTROL {
                KeyCode::Char('j')
            } else {
                KeyCode::Enter
            };
            assert_eq!(c.on_key(KeyEvent::new(code, m), &[], &[]), Out::Newline);
            type_str(&mut c, "b", &[]);
            assert_eq!(c.text(), "a\nb", "{m:?}");
        }
    }

    #[test]
    fn slash_popup_filters_and_tab_completes() {
        let cmds = registry(&[]);
        let mut c = Composer::new();
        type_str(&mut c, "/mod", &cmds);
        let pop = c.popup.as_ref().expect("popup");
        assert!(pop.items[0].insert.starts_with("/mod"));
        let want = format!("{} ", pop.items[0].insert);
        c.on_key(key(KeyCode::Tab), &cmds, &[]);
        assert_eq!(c.text(), want);
        assert!(c.popup.is_none());
    }

    #[test]
    fn enter_on_the_slash_popup_sends_what_is_typed_not_the_top_match() {
        let cmds = registry(&[]);
        let mut c = Composer::new();
        // A word that only fuzzy-matches `/model`: Enter sends it as typed, Tab completes.
        type_str(&mut c, "/mdl", &cmds);
        assert!(c.popup.is_some(), "the popup is open for this to matter");
        assert_eq!(c.on_key(key(KeyCode::Enter), &cmds, &[]), Out::Send);
        assert_eq!(c.text(), "/mdl");
    }

    fn spaced_files() -> Vec<String> {
        [
            "café.txt",
            "日本語 ファイル.md",
            "sub dir/",
            "sub dir/deep/",
            "sub dir/deep/x y.rs",
            "src/main.rs",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    fn type_with(c: &mut Composer, s: &str, files: &[String]) {
        for ch in s.chars() {
            c.on_key(key(KeyCode::Char(ch)), &[], files);
        }
    }

    #[test]
    fn a_name_with_spaces_is_inserted_quoted_and_a_quoted_folder_keeps_the_token_open() {
        let files = spaced_files();
        let mut c = Composer::new();
        type_with(&mut c, "see @x y", &files);
        // The plain token ended at the space; start over with the folder.
        let mut c2 = Composer::new();
        type_with(&mut c2, "see @sub", &files);
        c2.on_key(key(KeyCode::Tab), &[], &files);
        assert_eq!(c2.text(), "see @\"sub dir/");
        assert!(c2.popup.is_some(), "the popup follows into the folder");
        // Typing into the quoted token searches inside it, spaces and all.
        type_with(&mut c2, "deep/x y", &files);
        assert_eq!(
            c2.popup.as_ref().unwrap().items[0].insert,
            "sub dir/deep/x y.rs"
        );
        c2.on_key(key(KeyCode::Tab), &[], &files);
        assert_eq!(c2.text(), "see @\"sub dir/deep/x y.rs\" ");
        drop(c);
    }

    #[test]
    fn at_mentions_find_non_ascii_and_cjk_names_and_insert_them_whole() {
        let files = spaced_files();
        let mut c = Composer::new();
        type_with(&mut c, "@caf", &files);
        c.on_key(key(KeyCode::Tab), &[], &files);
        assert_eq!(c.text(), "@café.txt ");
        let mut c = Composer::new();
        type_with(&mut c, "@ファ", &files);
        c.on_key(key(KeyCode::Tab), &[], &files);
        assert_eq!(c.text(), "@\"日本語 ファイル.md\" ");
        let mut c = Composer::new();
        type_with(&mut c, "@日", &files);
        assert_eq!(
            c.popup.as_ref().unwrap().items[0].insert,
            "日本語 ファイル.md"
        );
    }

    #[test]
    fn a_big_index_is_searched_off_the_ui_thread_and_tab_waits_for_current_rows() {
        let mut c = Composer::new();
        c.remote_files = true;
        type_with(&mut c, "@ne", &[]);
        // Nothing is scored inline: the query is left for the worker and there are no rows yet.
        assert_eq!(c.want_files.as_deref(), Some("ne"));
        assert!(c.popup.is_none());
        let item = |p: &str| PItem {
            label: p.into(),
            insert: p.into(),
            ..PItem::default()
        };
        c.apply_hits("ne", vec![item("needle.txt")]);
        assert_eq!(c.popup.as_ref().unwrap().items[0].insert, "needle.txt");
        // Another key: the old rows stay up, but Tab must not complete them.
        type_with(&mut c, "x", &[]);
        assert_eq!(c.want_files.as_deref(), Some("nex"));
        c.on_key(key(KeyCode::Tab), &[], &[]);
        assert_eq!(c.text(), "@nex");
        // A reply for the old query is ignored; the current one lands.
        c.apply_hits("ne", vec![item("needle.txt")]);
        c.on_key(key(KeyCode::Tab), &[], &[]);
        assert_eq!(c.text(), "@nex", "still waiting for rows for nex");
        c.apply_hits("nex", vec![item("next.rs")]);
        c.on_key(key(KeyCode::Tab), &[], &[]);
        assert_eq!(c.text(), "@next.rs ");
    }

    #[test]
    fn a_small_paste_and_a_chip_both_lose_whole_escape_sequences_and_keep_tabs() {
        let mut c = Composer::new();
        c.paste("\x1b[31mRED\x1b[0m and\ttab\x07", std::path::Path::new("/"));
        assert_eq!(c.text(), "RED and\ttab");
        let mut c = Composer::new();
        let big: String = (0..20)
            .map(|i| format!("\x1b[1mline {i}\x1b[0m\x07\n"))
            .collect();
        c.paste(&big, std::path::Path::new("/"));
        assert!(c.text().starts_with("[paste "), "{}", c.text());
        let sent = c.expanded();
        assert!(!sent.contains('\x1b') && !sent.contains('\x07'), "{sent:?}");
        assert!(sent.contains("line 19\n"));
        assert!(!sent.contains("[1m"), "no stray sequence bodies: {sent:?}");
    }

    #[test]
    fn enter_completes_a_command_the_typed_word_is_the_start_of() {
        let cmds = registry(&[]);
        let mut c = Composer::new();
        type_str(&mut c, "/mod", &cmds);
        assert_eq!(c.on_key(key(KeyCode::Enter), &cmds, &[]), Out::None);
        assert_eq!(c.text(), "/mode ");
        assert_eq!(c.on_key(key(KeyCode::Enter), &cmds, &[]), Out::Send);
    }

    #[test]
    fn enter_completes_a_row_chosen_with_the_arrow_keys_and_the_next_enter_sends() {
        let cmds = registry(&[]);
        let mut c = Composer::new();
        type_str(&mut c, "/", &cmds);
        c.on_key(key(KeyCode::Down), &cmds, &[]);
        let want = format!("{} ", c.popup.as_ref().unwrap().items[1].insert);
        assert_eq!(c.on_key(key(KeyCode::Enter), &cmds, &[]), Out::None);
        assert_eq!(c.text(), want);
        assert_eq!(c.on_key(key(KeyCode::Enter), &cmds, &[]), Out::Send);
    }

    #[test]
    fn typing_after_the_arrow_keys_returns_enter_to_sending_what_is_typed() {
        let cmds = registry(&[]);
        let mut c = Composer::new();
        type_str(&mut c, "/", &cmds);
        c.on_key(key(KeyCode::Down), &cmds, &[]);
        type_str(&mut c, "cost", &cmds);
        assert_eq!(c.on_key(key(KeyCode::Enter), &cmds, &[]), Out::Send);
        assert_eq!(c.text(), "/cost");
    }

    #[test]
    fn a_complete_command_shows_its_argument_hint_as_a_ghost() {
        let cmds = registry(&[]);
        let mut c = Composer::new();
        type_str(&mut c, "/copy ", &cmds);
        assert_eq!(c.ghost.as_deref(), Some("[n]"));
        type_str(&mut c, "2", &cmds);
        assert_eq!(c.ghost, None);
    }

    #[test]
    fn file_popup_after_at_and_directories_keep_the_token_open() {
        let files = vec![
            "src/main.rs".to_string(),
            "src/lib.rs".to_string(),
            "README.md".to_string(),
            "src/".to_string(),
        ];
        let mut c = Composer::new();
        for ch in "see @lib".chars() {
            c.on_key(key(KeyCode::Char(ch)), &[], &files);
        }
        let pop = c.popup.as_ref().expect("popup");
        assert_eq!(pop.items[0].insert, "src/lib.rs");
        c.on_key(key(KeyCode::Tab), &[], &files);
        assert_eq!(c.text(), "see @src/lib.rs ");
        let mut c = Composer::new();
        for ch in "@sr".chars() {
            c.on_key(key(KeyCode::Char(ch)), &[], &files);
        }
        assert_eq!(c.popup.as_ref().unwrap().items[0].insert, "src/");
        c.on_key(key(KeyCode::Tab), &[], &files);
        assert_eq!(c.text(), "@src/");
        assert!(c.popup.is_some(), "the popup follows into the directory");
    }

    #[test]
    fn the_busy_composer_says_what_enter_and_ctrl_s_do() {
        let (ph, hint) = placeholder(true, 0, 100);
        assert!(ph.contains("enter queues") && ph.contains("ctrl+s steers"));
        assert!(hint.contains("queues"));
        let (ph, _) = placeholder(true, 2, 100);
        assert!(ph.contains("up takes the last back"));
        assert!(placeholder(false, 0, 100).0.starts_with("Message"));
    }
}
