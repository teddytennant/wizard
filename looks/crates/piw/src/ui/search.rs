// OWNER: search (transcript search, `ctrl+shift+f`)
//! Pi's transcript search: a port of pi-tui's `alt-screen-search.js` (the corpus, the matcher and
//! the box) and the parts of `tui-alt-screen.js` that drive it (`refreshSearch`,
//! `applySearchHighlights`, the overlay's place). The corpus is the rendered transcript text with
//! every run of whitespace, line breaks included, collapsed to one space, so a match can span
//! rows; each match is a list of row segments in cell columns. Spec 2.2.

use std::hash::{Hash, Hasher};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use regex::RegexBuilder;
use tuikit::width::{display_width, grapheme_width};
use unicode_segmentation::UnicodeSegmentation;

use super::selectors::Input;
use crate::theme::{PiTheme, Tok};

/// One word of the corpus and where it is on screen.
#[derive(Clone, Debug)]
struct Span_ {
    text_start: usize,
    text_end: usize,
    row: usize,
    start_col: usize,
    end_col: usize,
    /// ASCII runs map text offsets to columns one to one; a wide glyph is a span of its own.
    linear: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Seg {
    pub row: usize,
    pub start_col: usize,
    pub end_col: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Match {
    pub segs: Vec<Seg>,
}

impl Match {
    fn key(&self) -> String {
        match (self.segs.first(), self.segs.last()) {
            (Some(a), Some(b)) => format!("{}:{}:{}:{}", a.row, a.start_col, b.row, b.end_col),
            _ => String::new(),
        }
    }
}

struct Corpus {
    text: String,
    spans: Vec<Span_>,
}

/// `buildSearchCorpus`.
fn build_corpus(lines: &[String]) -> Corpus {
    let mut text = String::new();
    let mut spans: Vec<Span_> = Vec::new();
    let mut pending_sep = false;
    for (row, line) in lines.iter().enumerate() {
        let mut col = 0usize;
        let ascii = line.bytes().all(|b| (0x20..=0x7e).contains(&b));
        let mut push = |piece: &str,
                        width: usize,
                        linear: bool,
                        col: &mut usize,
                        pending: &mut bool,
                        text: &mut String| {
            if *pending {
                text.push(' ');
                *pending = false;
            }
            let start = text.len();
            text.push_str(piece);
            spans.push(Span_ {
                text_start: start,
                text_end: text.len(),
                row,
                start_col: *col,
                end_col: *col + width,
                linear,
            });
            *col += width;
        };
        if ascii {
            let b = line.as_bytes();
            let mut i = 0;
            while i < b.len() {
                if b[i] == b' ' {
                    if !text.is_empty() {
                        pending_sep = true;
                    }
                    col += 1;
                    i += 1;
                    continue;
                }
                let mut end = i + 1;
                while end < b.len() && b[end] != b' ' {
                    end += 1;
                }
                push(
                    &line[i..end],
                    end - i,
                    true,
                    &mut col,
                    &mut pending_sep,
                    &mut text,
                );
                i = end;
            }
        } else {
            for g in line.graphemes(true) {
                let w = grapheme_width(g);
                if g.chars().all(char::is_whitespace) {
                    if !text.is_empty() {
                        pending_sep = true;
                    }
                    col += w;
                    continue;
                }
                push(g, w, false, &mut col, &mut pending_sep, &mut text);
            }
        }
        if !text.is_empty() {
            pending_sep = true;
        }
    }
    Corpus { text, spans }
}

fn normalize_query(q: &str) -> String {
    q.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `findSearchCorpusMatches`: case-insensitive, non-overlapping, the whole query literally.
fn find_matches(corpus: &Corpus, query: &str) -> Vec<Match> {
    if query.is_empty() {
        return Vec::new();
    }
    let Ok(re) = RegexBuilder::new(&regex::escape(query))
        .case_insensitive(true)
        .unicode(true)
        .build()
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut span_index = 0usize;
    for m in re.find_iter(&corpus.text) {
        let (start, end) = (m.start(), m.end());
        while span_index < corpus.spans.len() && corpus.spans[span_index].text_end <= start {
            span_index += 1;
        }
        let mut segs: Vec<Seg> = Vec::new();
        for s in &corpus.spans[span_index..] {
            if s.text_start >= end {
                break;
            }
            if s.text_end <= start {
                continue;
            }
            let (a, b) = if s.linear {
                (
                    s.start_col + start.max(s.text_start) - s.text_start,
                    s.start_col + end.min(s.text_end) - s.text_start,
                )
            } else {
                (s.start_col, s.end_col)
            };
            match segs.last_mut() {
                Some(p) if p.row == s.row && a <= p.end_col => p.end_col = p.end_col.max(b),
                _ => segs.push(Seg {
                    row: s.row,
                    start_col: a,
                    end_col: b,
                }),
            }
        }
        while span_index < corpus.spans.len() && corpus.spans[span_index].text_end <= end {
            span_index += 1;
        }
        if !segs.is_empty() {
            out.push(Match { segs });
        }
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// The query changed: pick the first match at or after where the view was.
    Query,
    Next,
    Previous,
    Retain,
}

pub struct Search {
    input: Input,
    /// The normalised query the matches are for.
    query: String,
    matches: Vec<Match>,
    selected: Option<usize>,
    selected_key: Option<String>,
    mode: Mode,
    anchor_row: usize,
    corpus: Option<Corpus>,
    source: u64,
}

/// What a key did to the search.
#[derive(Debug, PartialEq, Eq)]
pub enum SearchKey {
    Close,
    /// Consumed (the query or the selection changed, or it was ignored).
    Handled,
    /// A key that scrolls the transcript; the app handles it as usual.
    Pass,
}

impl Search {
    /// Open the box. `scroll_top` is where the view is, which the first selection starts from.
    pub fn open(scroll_top: usize) -> Search {
        Search {
            input: Input::new(),
            query: String::new(),
            matches: Vec::new(),
            selected: None,
            selected_key: None,
            mode: Mode::Query,
            anchor_row: scroll_top,
            corpus: None,
            source: 0,
        }
    }

    pub fn result(&self) -> (Option<usize>, usize) {
        (self.selected, self.matches.len())
    }

    pub fn matches(&self) -> &[Match] {
        &self.matches
    }

    pub fn query(&self) -> &str {
        self.input.text()
    }

    /// `ctrl+shift+f`, as terminals report it: `f` with ctrl and shift (or an uppercase `F`).
    pub fn is_toggle(key: &KeyEvent) -> bool {
        let m = key.modifiers;
        matches!(key.code, KeyCode::Char('f' | 'F'))
            && m.contains(KeyModifiers::CONTROL)
            && m.contains(KeyModifiers::SHIFT)
    }

    /// `tui.altScreen.searchNext` (`enter`, `ctrl+g`) and `searchPrevious` (`shift+enter`,
    /// `ctrl+shift+g`), `searchClose` (`escape`); everything else edits the query.
    pub fn key(
        &mut self,
        key: KeyEvent,
        scroll_top: usize,
        passes: impl Fn(&KeyEvent) -> bool,
    ) -> SearchKey {
        let m = key.modifiers;
        let ctrl = m.contains(KeyModifiers::CONTROL);
        let shift = m.contains(KeyModifiers::SHIFT);
        match key.code {
            KeyCode::Esc => return SearchKey::Close,
            KeyCode::Enter if shift => self.navigate(-1),
            KeyCode::Enter => self.navigate(1),
            KeyCode::Char('g' | 'G') if ctrl && shift => self.navigate(-1),
            KeyCode::Char('g') if ctrl => self.navigate(1),
            _ if passes(&key) => return SearchKey::Pass,
            _ => {
                let before = self.input.text().to_string();
                // the query box is a one-line input; ctrl+c clears nothing, the keys it does not know are ignored
                if self.input.key(key) && self.input.text() != before {
                    let selected = self.selected.and_then(|i| self.matches.get(i));
                    self.anchor_row = selected
                        .and_then(|m| m.segs.first())
                        .map_or(scroll_top, |s| s.row);
                    self.mode = Mode::Query;
                    self.selected = None;
                }
            }
        }
        SearchKey::Handled
    }

    pub fn paste(&mut self, text: &str, scroll_top: usize) {
        let before = self.input.text().to_string();
        self.input.paste(&text.replace(['\n', '\r'], " "));
        if self.input.text() != before {
            let selected = self.selected.and_then(|i| self.matches.get(i));
            self.anchor_row = selected
                .and_then(|m| m.segs.first())
                .map_or(scroll_top, |s| s.row);
            self.mode = Mode::Query;
            self.selected = None;
        }
    }

    fn navigate(&mut self, dir: i32) {
        if self.input.text().is_empty() {
            return;
        }
        self.mode = if dir < 0 { Mode::Previous } else { Mode::Next };
    }

    /// `refreshSearch`: find the matches in the rendered rows, pick the selection, and say where
    /// to scroll, which is where the view already is when the selection is in it.
    pub fn refresh<'a>(
        &mut self,
        lines: impl Iterator<Item = &'a Line<'static>>,
        view_top: usize,
        view_h: usize,
    ) -> Option<usize> {
        let query = self.input.text().to_string();
        if query.trim().is_empty() {
            self.matches.clear();
            self.selected = None;
            self.selected_key = None;
            self.mode = Mode::Retain;
            self.query.clear();
            return None;
        }
        let plain: Vec<String> = lines
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        let source = {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            plain.hash(&mut h);
            h.finish()
        };
        let source_changed = self.corpus.is_none() || source != self.source;
        if source_changed {
            self.corpus = Some(build_corpus(&plain));
            self.source = source;
        }
        let normalized = normalize_query(&query);
        let changed = source_changed || normalized != self.query;
        if changed {
            let Some(corpus) = &self.corpus else {
                return None;
            };
            self.matches = find_matches(corpus, &normalized);
            self.query = normalized;
        }
        if !changed && self.mode == Mode::Retain {
            return None;
        }
        let reveal = self.mode != Mode::Retain;
        let n = self.matches.len();
        let selected_index = self.selected.map_or(-1, |i| i as isize);
        let exact: isize = if changed {
            self.selected_key
                .as_ref()
                .and_then(|k| self.matches.iter().position(|m| &m.key() == k))
                .map_or(-1, |i| i as isize)
        } else {
            selected_index
        };
        let mut pick: Option<usize> = None;
        if n > 0 {
            let base = if exact >= 0 {
                exact
            } else {
                selected_index.min(n as isize - 1)
            };
            pick = Some(match self.mode {
                Mode::Query => {
                    let low = self
                        .matches
                        .partition_point(|m| m.segs.first().map_or(0, |s| s.row) < self.anchor_row);
                    if low < n {
                        low
                    } else {
                        0
                    }
                }
                Mode::Next => {
                    if base < 0 {
                        0
                    } else {
                        (base as usize + 1) % n
                    }
                }
                Mode::Previous => {
                    if base < 0 {
                        n - 1
                    } else {
                        (base as usize + n - 1) % n
                    }
                }
                Mode::Retain => {
                    if exact >= 0 {
                        exact as usize
                    } else {
                        (selected_index.max(0) as usize).min(n - 1)
                    }
                }
            });
        }
        self.selected = pick;
        self.selected_key = pick.map(|i| self.matches[i].key());
        self.mode = Mode::Retain;
        if !reveal {
            return None;
        }
        let m = &self.matches[pick?];
        let (first, last) = (m.segs.first()?, m.segs.last()?);
        if view_h == 0 {
            return None;
        }
        // `scrollTo(target, {disableFollow: true})` runs for every reveal, in view or not
        let bottom = view_top + view_h - 1;
        Some(if first.row < view_top || last.row > bottom {
            first.row.saturating_sub(view_h / 3)
        } else {
            view_top
        })
    }

    /// Paint the matches that are in view: underline over `searchMatchBg` and `searchMatchText`,
    /// the current one bold and reversed (`tui-renderer.js`).
    pub fn highlight(&self, buf: &mut Buffer, top: usize, view_h: usize, width: u16, th: &PiTheme) {
        let Some(sel) = self.selected else { return };
        let bg = th.paint(Tok::SearchMatchBg);
        let fg = th.paint(Tok::SearchMatchText);
        let low = self
            .matches
            .partition_point(|m| m.segs.last().map_or(0, |s| s.row) < top);
        for (i, m) in self.matches.iter().enumerate().skip(low) {
            if m.segs.first().is_some_and(|s| s.row >= top + view_h) {
                break;
            }
            for s in &m.segs {
                if s.row < top || s.row >= top + view_h {
                    continue;
                }
                let y = (s.row - top) as u16;
                let (a, b) = (s.start_col as u16, (s.end_col as u16).min(width));
                for x in a..b {
                    let Some(cell) = buf.cell_mut((x, y)) else {
                        continue;
                    };
                    if bg.color != Color::Reset {
                        cell.set_bg(bg.color);
                    }
                    if fg.color != Color::Reset {
                        cell.set_fg(fg.color);
                    }
                    let mut add = if i == sel {
                        Modifier::BOLD | Modifier::REVERSED
                    } else {
                        Modifier::UNDERLINED
                    };
                    if fg.faint {
                        add |= Modifier::DIM;
                    }
                    cell.modifier.insert(add);
                }
            }
        }
    }

    // ---- the box -----------------------------------------------------------------------------

    /// The overlay: `anchor: top-right`, 40% of the width but at least 32, a margin of one.
    pub fn overlay_rect(screen_w: u16) -> (u16, u16, u16) {
        let w = ((screen_w as usize * 40) / 100)
            .max(32)
            .min(screen_w.saturating_sub(2) as usize)
            .max(1);
        (screen_w.saturating_sub(1 + w as u16), 1, w as u16)
    }

    /// `AltScreenSearchComponent.render`.
    pub fn render(&self, width: u16) -> Vec<Line<'static>> {
        let dim = Style::default().add_modifier(Modifier::DIM);
        let safe = width.max(1) as usize;
        if safe == 1 {
            return vec![Line::from("┌"), Line::from("│"), Line::from("└")];
        }
        let inner = safe.saturating_sub(2);
        let query = self.input.text();
        let result = if query.is_empty() {
            String::new()
        } else if self.matches.is_empty() {
            "No matches".to_string()
        } else {
            format!(
                "{}/{}",
                self.selected.map_or(0, |i| i + 1),
                self.matches.len()
            )
        };
        let result_space = inner.saturating_sub(3);
        let visible = super::cut(&result, result_space);
        let (result_spans, result_w) = if visible.is_empty() {
            (Vec::new(), 0)
        } else {
            let t = format!(" {visible} ");
            let w = display_width(&t);
            (vec![Span::styled(t, dim)], w)
        };
        let input_w = inner.saturating_sub(result_w);
        let mut content = self.input_row(input_w);
        let used: usize = content.iter().map(|s| display_width(&s.content)).sum();
        if used < input_w {
            content.push(Span::raw(" ".repeat(input_w - used)));
        }
        content.extend(result_spans);

        let mut prev = "↑ Shift+Enter".to_string();
        let mut next = "↓ Enter".to_string();
        let mut sep = " · ".to_string();
        let outer_gap = 1usize;
        let available = inner.saturating_sub(outer_gap * 2 + 1);
        let mut controls = display_width(&prev) + display_width(&sep) + display_width(&next);
        if controls > available {
            prev = "↑".into();
            next = "↓".into();
            sep = " ".into();
            controls = display_width(&prev) + display_width(&sep) + display_width(&next);
        }
        let show = controls <= available;
        let buttons = if show {
            format!("{prev}{sep}{next}")
        } else {
            String::new()
        };
        let gaps = if show { outer_gap * 2 } else { 0 };
        let right_rule = usize::from(show && inner > controls + gaps);
        let left_rule = inner.saturating_sub(if show { controls } else { 0 } + gaps + right_rule);
        let top = format!("┌{}┐", "─".repeat(inner));
        let bottom = format!(
            "└{}{}{}{}{}┘",
            "─".repeat(left_rule),
            if show { " " } else { "" },
            buttons,
            if show { " " } else { "" },
            "─".repeat(right_rule)
        );
        let mut mid = vec![Span::raw("│")];
        mid.extend(content);
        mid.push(Span::raw("│"));
        vec![Line::from(top), Line::from(mid), Line::from(bottom)]
    }

    /// pi-tui `Input.render` with the prompt ` ` and the placeholder `Find in transcript`.
    fn input_row(&self, width: usize) -> Vec<Span<'static>> {
        let rev = Style::default().add_modifier(Modifier::REVERSED);
        let dim = Style::default().add_modifier(Modifier::DIM);
        let prompt = " ";
        let avail = width.saturating_sub(display_width(prompt));
        if avail == 0 {
            return vec![Span::raw(super::cut(prompt, width))];
        }
        let value = self.input.text();
        if value.is_empty() {
            let ph = super::cut("Find in transcript", avail);
            let mut g = ph.graphemes(true);
            let at = g.next().unwrap_or(" ").to_string();
            let rest: String = g.collect();
            let mut v = vec![Span::raw(prompt), Span::styled(at.clone(), rev.patch(dim))];
            if !rest.is_empty() {
                v.push(Span::styled(rest.clone(), dim));
            }
            let used = display_width(prompt) + display_width(&at) + display_width(&rest);
            if used < width {
                v.push(Span::raw(" ".repeat(width - used)));
            }
            return v;
        }
        let cursor = self.input.cursor();
        let total = display_width(value);
        let (visible, cursor_in): (String, usize) = if total < avail {
            (value.to_string(), cursor)
        } else {
            let scroll = if cursor == value.len() {
                avail - 1
            } else {
                avail
            };
            let cursor_col = display_width(&value[..cursor]);
            if scroll == 0 {
                (String::new(), 0)
            } else {
                let half = scroll / 2;
                let start_col = if cursor_col < half {
                    0
                } else if cursor_col > total.saturating_sub(half) {
                    total.saturating_sub(scroll)
                } else {
                    cursor_col.saturating_sub(half)
                };
                let vis = slice_cols(value, start_col, scroll);
                let before = slice_cols(value, start_col, cursor_col.saturating_sub(start_col));
                let len = before.len();
                (vis, len)
            }
        };
        let before = &visible[..cursor_in.min(visible.len())];
        let after = &visible[cursor_in.min(visible.len())..];
        let mut g = after.graphemes(true);
        let at = g.next().unwrap_or(" ").to_string();
        let rest: String = g.collect();
        let mut v = vec![
            Span::raw(prompt),
            Span::raw(before.to_string()),
            Span::styled(at.clone(), rev),
        ];
        if !rest.is_empty() {
            v.push(Span::raw(rest.clone()));
        }
        let used = display_width(prompt)
            + display_width(before)
            + display_width(&at)
            + display_width(&rest);
        if used < width {
            v.push(Span::raw(" ".repeat(width - used)));
        }
        v
    }

    /// Draw the box over the top right of the viewport.
    pub fn draw(&self, buf: &mut Buffer, screen_w: u16) {
        let (x, y, w) = Search::overlay_rect(screen_w);
        for (i, line) in self.render(w).iter().enumerate() {
            let row = y + i as u16;
            if row >= buf.area.y + buf.area.height {
                break;
            }
            // the box replaces what is under it; its cells carry no style of their own
            for dx in 0..w {
                if let Some(c) = buf.cell_mut((x + dx, row)) {
                    c.reset();
                }
            }
            buf.set_line(x, row, line, w);
        }
    }
}

/// `sliceByColumn(text, start, len)`: the graphemes that start at or after column `start` and fit
/// in `len` cells.
fn slice_cols(text: &str, start: usize, len: usize) -> String {
    let mut out = String::new();
    let (mut col, mut used) = (0usize, 0usize);
    for g in text.graphemes(true) {
        let w = grapheme_width(g);
        if col >= start && used + w <= len {
            out.push_str(g);
            used += w;
        }
        col += w;
        if used >= len {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corpus(lines: &[&str]) -> Corpus {
        build_corpus(&lines.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn whitespace_collapses_and_matches_span_rows() {
        let c = corpus(&["hello   world", "  second  line"]);
        assert_eq!(c.text, "hello world second line");
        let m = find_matches(&c, "world second");
        assert_eq!(m.len(), 1);
        assert_eq!(
            m[0].segs,
            vec![
                Seg {
                    row: 0,
                    start_col: 8,
                    end_col: 13
                },
                Seg {
                    row: 1,
                    start_col: 2,
                    end_col: 8
                }
            ]
        );
    }

    #[test]
    fn matching_is_case_insensitive_and_literal() {
        let c = corpus(&["A.b a+b A.B"]);
        assert_eq!(find_matches(&c, "a.b").len(), 2);
        assert_eq!(find_matches(&c, "a+b").len(), 1);
        assert!(find_matches(&c, "").is_empty());
    }

    #[test]
    fn wide_glyphs_are_columns_not_bytes() {
        let c = corpus(&["日本 語x"]);
        let m = find_matches(&c, "語");
        assert_eq!(
            m[0].segs,
            vec![Seg {
                row: 0,
                start_col: 5,
                end_col: 7
            }]
        );
    }
}
