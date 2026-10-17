//! Transcript lines and the word wrap jcode applies to them.

use ratatui::style::Style;
use ratatui::text::Span;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// What a wrapped continuation row starts with.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Hang {
    /// Spaces as wide as the line's own leading whitespace.
    #[default]
    Auto,
    /// Nothing: continuation rows start at the left edge.
    None,
    /// These spans, repeated on every continuation row (a list indent, a `│ ` gutter).
    Spans(Vec<Span<'static>>),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RLine {
    pub spans: Vec<Span<'static>>,
    pub hang: Hang,
    /// The number of the user prompt this row belongs to, on rows of its first line.
    pub prompt: Option<usize>,
    /// Cut with an ellipsis instead of wrapping.
    pub nowrap: bool,
}

impl RLine {
    pub fn new(spans: Vec<Span<'static>>) -> Self {
        RLine {
            spans,
            ..Default::default()
        }
    }

    pub fn blank() -> Self {
        RLine::default()
    }

    pub fn styled(text: impl Into<String>, style: Style) -> Self {
        RLine::new(vec![Span::styled(text.into(), style)])
    }

    pub fn hang(mut self, hang: Hang) -> Self {
        self.hang = hang;
        self
    }

    pub fn nowrap(mut self) -> Self {
        self.nowrap = true;
        self
    }

    pub fn width(&self) -> usize {
        spans_width(&self.spans)
    }

    pub fn text(&self) -> String {
        self.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    pub fn is_blank(&self) -> bool {
        self.spans.iter().all(|s| s.content.trim().is_empty())
    }
}

pub fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|s| s.content.width()).sum()
}

struct Token {
    word: Vec<(char, Style)>,
    spaces: Vec<(char, Style)>,
}

impl Token {
    fn word_width(&self) -> usize {
        self.word.iter().map(|(c, _)| c.width().unwrap_or(0)).sum()
    }

    fn space_width(&self) -> usize {
        self.spaces.len()
    }
}

fn tokenize(spans: &[Span<'static>]) -> Vec<Token> {
    let mut out: Vec<Token> = Vec::new();
    let mut cur = Token {
        word: Vec::new(),
        spaces: Vec::new(),
    };
    for s in spans {
        for c in s.content.chars() {
            if c == ' ' {
                cur.spaces.push((c, s.style));
            } else {
                if !cur.spaces.is_empty() {
                    out.push(std::mem::replace(
                        &mut cur,
                        Token {
                            word: Vec::new(),
                            spaces: Vec::new(),
                        },
                    ));
                }
                cur.word.push((c, s.style));
            }
        }
    }
    if !cur.word.is_empty() || !cur.spaces.is_empty() {
        out.push(cur);
    }
    out
}

fn push_chars(row: &mut Vec<Span<'static>>, chars: &[(char, Style)]) {
    for (c, style) in chars {
        match row.last_mut() {
            Some(last) if last.style == *style => last.content.to_mut().push(*c),
            _ => row.push(Span::styled(c.to_string(), *style)),
        }
    }
}

fn leading_spaces(spans: &[Span<'_>]) -> usize {
    let mut n = 0;
    for s in spans {
        for c in s.content.chars() {
            if c == ' ' {
                n += 1;
            } else {
                return n;
            }
        }
    }
    n
}

/// Word wrap at `width` cells. A word moves to the next row together with the spaces after it,
/// so a word that would fit only without its trailing space still wraps; a word wider than the
/// row is split by character.
pub fn wrap(line: &RLine, width: usize) -> Vec<Vec<Span<'static>>> {
    if width == 0 || line.width() <= width {
        return vec![line.spans.clone()];
    }
    let prefix: Vec<Span<'static>> = match &line.hang {
        Hang::None => Vec::new(),
        Hang::Spans(s) => s.clone(),
        Hang::Auto => match leading_spaces(&line.spans) {
            0 => Vec::new(),
            n => vec![Span::raw(" ".repeat(n))],
        },
    };
    let mut pw = spans_width(&prefix);
    let prefix = if pw == 0 || pw >= width {
        pw = 0;
        Vec::new()
    } else {
        prefix
    };
    let mut rows: Vec<Vec<Span<'static>>> = Vec::new();
    let mut row: Vec<Span<'static>> = Vec::new();
    let mut w = 0usize;
    let mut has = false;
    let mut seed = false;
    macro_rules! flush {
        () => {{
            rows.push(std::mem::take(&mut row));
            w = 0;
            has = false;
            seed = !prefix.is_empty();
        }};
    }
    macro_rules! seed {
        () => {{
            if seed {
                row.extend(prefix.iter().cloned());
                w = pw;
                seed = false;
            }
        }};
    }
    for t in tokenize(&line.spans) {
        let (ww, sw) = (t.word_width(), t.space_width());
        if w + ww + sw > width && has {
            flush!();
        }
        if ww > width {
            for (c, style) in &t.word {
                seed!();
                let cw = c.width().unwrap_or(0);
                if w + cw > width && w > 0 {
                    if has {
                        flush!();
                    }
                    seed!();
                }
                push_chars(&mut row, &[(*c, *style)]);
                if w + cw > pw {
                    has = true;
                }
                w += cw;
            }
        } else {
            seed!();
            push_chars(&mut row, &t.word);
            if w + ww > pw {
                has = true;
            }
            w += ww;
        }
        push_chars(&mut row, &t.spaces);
        if sw > 0 && w + sw > pw && ww > 0 {
            has = true;
        }
        w += sw;
    }
    if !row.is_empty() && has {
        rows.push(row);
    }
    if rows.is_empty() {
        rows.push(Vec::new());
    }
    rows
}

/// Cut `spans` to `max` cells, ending in `…` styled like the last kept span.
pub fn truncate_spans(spans: &[Span<'static>], max: usize) -> Vec<Span<'static>> {
    if spans_width(spans) <= max {
        return spans.to_vec();
    }
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut w = 0usize;
    let budget = max.saturating_sub(1);
    let mut last = Style::default();
    'outer: for s in spans {
        let mut kept = String::new();
        for c in s.content.chars() {
            let cw = c.width().unwrap_or(0);
            if w + cw > budget {
                if !kept.is_empty() {
                    last = s.style;
                    out.push(Span::styled(kept, s.style));
                }
                break 'outer;
            }
            kept.push(c);
            w += cw;
        }
        if !kept.is_empty() {
            last = s.style;
            out.push(Span::styled(kept, s.style));
        }
    }
    if max > 0 {
        out.push(Span::styled("…", last));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(text: &str, width: usize) -> Vec<String> {
        wrap(&RLine::new(vec![Span::raw(text.to_string())]), width)
            .into_iter()
            .map(|r| r.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn a_word_wraps_with_its_trailing_space() {
        // "bb" fits in the 5 cells left, "bb " does not
        assert_eq!(rows("aaa bb cc", 6), ["aaa ", "bb cc"]);
        assert_eq!(rows("aaa bb", 6), ["aaa bb"]);
    }

    #[test]
    fn long_words_split_by_character() {
        assert_eq!(rows("abcdefgh", 3), ["abc", "def", "gh"]);
    }

    #[test]
    fn leading_whitespace_hangs() {
        assert_eq!(rows("  aaa bbb ccc", 8), ["  aaa ", "  bbb ", "  ccc"]);
    }

    #[test]
    fn truncation_keeps_the_last_style() {
        let s = vec![Span::raw("hello world")];
        let t = truncate_spans(&s, 6);
        assert_eq!(
            t.iter().map(|s| s.content.as_ref()).collect::<String>(),
            "hello…"
        );
    }
}
