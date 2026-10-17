// OWNER: renderer
//! Word wrapping for styled lines, shaped like Codex's `wrapping.rs` over `textwrap`:
//! first-fit, words split after hyphens between alphanumerics, long words broken by grapheme,
//! trailing spaces trimmed, a styled indent on the first and on the following rows.

use std::ops::Range;

use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;

pub fn width_of(s: &str) -> usize {
    // Codex counts the halfwidth sound marks (U+FF9E, U+FF9F) as a cell of their own.
    crate::width::display_width(s)
}

pub fn line_width(line: &Line<'_>) -> usize {
    line.spans.iter().map(|s| width_of(&s.content)).sum()
}

/// Wrapping knobs. `break_words` off keeps an overlong token on one row (URLs).
#[derive(Clone)]
pub struct WrapOpts {
    pub width: usize,
    pub initial_indent: Line<'static>,
    pub subsequent_indent: Line<'static>,
    pub break_words: bool,
    pub hyphen_split: bool,
    /// Break words at Unicode line-break opportunities (UAX #14), as textwrap's default
    /// separator does, instead of at spaces only. On by default, like Codex; URL wrapping turns it off.
    pub uax14: bool,
}

impl WrapOpts {
    pub fn new(width: usize) -> Self {
        Self {
            width,
            initial_indent: Line::default(),
            subsequent_indent: Line::default(),
            break_words: true,
            hyphen_split: true,
            uax14: true,
        }
    }
    pub fn uax14(mut self, on: bool) -> Self {
        self.uax14 = on;
        self
    }
    pub fn initial_indent(mut self, l: Line<'static>) -> Self {
        self.initial_indent = l;
        self
    }
    pub fn subsequent_indent(mut self, l: Line<'static>) -> Self {
        self.subsequent_indent = l;
        self
    }
}

/// A word with the whitespace that follows it, as byte ranges into the text.
struct Word {
    start: usize,
    /// End of the visible part.
    end: usize,
}

/// Word boundaries: runs of non-space text, or, with `uax14`, the segments between line-break
/// opportunities with their trailing spaces left out of the visible part.
fn base_words(text: &str, uax14: bool) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    if uax14 {
        let mut start = 0;
        for (pos, _) in unicode_linebreak::linebreaks(text) {
            if pos <= start {
                continue;
            }
            let end = start + text[start..pos].trim_end_matches([' ', '\n']).len();
            out.push((start, end));
            start = pos;
        }
        if start < text.len() {
            out.push((start, text.len()));
        }
        return out;
    }
    let b = text.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let start = i;
        while i < b.len() && b[i] != b' ' {
            i += 1;
        }
        let end = i;
        while i < b.len() && b[i] == b' ' {
            i += 1;
        }
        out.push((start, end));
    }
    out
}

fn split_words(text: &str, hyphen: bool, uax14: bool) -> Vec<Word> {
    let mut words = Vec::new();
    for (start, end) in base_words(text, uax14) {
        if hyphen {
            // Split after a '-' that sits between two alphanumerics, like textwrap's HyphenSplitter.
            let word = &text[start..end];
            let chars: Vec<(usize, char)> = word.char_indices().collect();
            let mut from = start;
            for k in 1..chars.len().saturating_sub(1) {
                if chars[k].1 == '-'
                    && chars[k - 1].1.is_alphanumeric()
                    && chars[k + 1].1.is_alphanumeric()
                {
                    let cut = start + chars[k].0 + 1;
                    words.push(Word {
                        start: from,
                        end: cut,
                    });
                    from = cut;
                }
            }
            words.push(Word { start: from, end });
        } else {
            words.push(Word { start, end });
        }
    }
    words
}

/// Wrap `text` to `width` columns; returned ranges exclude trailing whitespace. An empty text
/// gives one empty range, like textwrap.
pub fn wrap_ranges_trim(
    text: &str,
    width: usize,
    break_words: bool,
    hyphen: bool,
) -> Vec<Range<usize>> {
    wrap_ranges_trim_with(text, width, break_words, hyphen, false)
}

pub fn wrap_ranges_trim_with(
    text: &str,
    width: usize,
    break_words: bool,
    hyphen: bool,
    uax14: bool,
) -> Vec<Range<usize>> {
    let width = width.max(1);
    if text.is_empty() {
        return std::iter::once(0..0).collect();
    }
    let words = split_words(text, hyphen, uax14);
    let mut out: Vec<Range<usize>> = Vec::new();
    let mut line_start: Option<usize> = None;
    let mut line_end = 0usize;
    let mut cur_w = 0usize;
    for w in &words {
        let vis = &text[w.start..w.end];
        let vw = width_of(vis);
        let gap_w = line_start.map_or(0, |_| width_of(&text[line_end..w.start]));
        if let Some(ls) = line_start {
            if cur_w + gap_w + vw <= width {
                cur_w += gap_w + vw;
                line_end = w.end;
                continue;
            }
            out.push(ls..line_end);
        }
        if vw <= width || !break_words {
            line_start = Some(w.start);
            line_end = w.end;
            cur_w = vw;
        } else {
            // Break an overlong word by grapheme.
            let mut seg_start = w.start;
            let mut seg_w = 0usize;
            let mut off = w.start;
            for g in vis.graphemes(true) {
                let gw = width_of(g);
                if seg_w + gw > width && seg_w > 0 {
                    out.push(seg_start..off);
                    seg_start = off;
                    seg_w = 0;
                }
                seg_w += gw;
                off += g.len();
            }
            line_start = Some(seg_start);
            line_end = off;
            cur_w = seg_w;
        }
    }
    if let Some(ls) = line_start {
        out.push(ls..line_end);
    }
    if out.is_empty() {
        out.push(0..0);
    }
    out
}

fn slice_spans(
    line: &Line<'static>,
    bounds: &[(Range<usize>, usize)],
    range: &Range<usize>,
) -> Vec<Span<'static>> {
    let mut acc = Vec::new();
    for (r, i) in bounds {
        if r.end <= range.start {
            continue;
        }
        if r.start >= range.end {
            break;
        }
        let s = range.start.max(r.start);
        let e = range.end.min(r.end);
        if e > s {
            let content = &line.spans[*i].content[s - r.start..e - r.start];
            acc.push(Span::styled(content.to_string(), line.spans[*i].style));
        }
    }
    acc
}

/// Wrap one styled line. Mirrors Codex's `word_wrap_line`: the first row has
/// `initial_indent`, later rows `subsequent_indent`, and spaces at a break are dropped.
pub fn word_wrap_line(line: &Line<'static>, opts: &WrapOpts) -> Vec<Line<'static>> {
    let mut flat = String::new();
    let mut bounds = Vec::new();
    for (i, s) in line.spans.iter().enumerate() {
        let start = flat.len();
        flat.push_str(&s.content);
        bounds.push((start..flat.len(), i));
    }
    let first_avail = opts
        .width
        .saturating_sub(line_width(&opts.initial_indent))
        .max(1);
    let first = wrap_ranges_trim_with(
        &flat,
        first_avail,
        opts.break_words,
        opts.hyphen_split,
        opts.uax14,
    );
    let first_range = first[0].clone();
    let mut out = Vec::new();
    let mut spans = opts.initial_indent.spans.clone();
    spans.extend(slice_spans(line, &bounds, &first_range));
    out.push(Line::from(spans).style(line.style));

    let mut base = first_range.end;
    base += flat[base..].chars().take_while(|c| *c == ' ').count();
    if base >= flat.len() {
        return out;
    }
    let sub_avail = opts
        .width
        .saturating_sub(line_width(&opts.subsequent_indent))
        .max(1);
    for r in wrap_ranges_trim_with(
        &flat[base..],
        sub_avail,
        opts.break_words,
        opts.hyphen_split,
        opts.uax14,
    ) {
        if r.is_empty() {
            continue;
        }
        let range = (r.start + base)..(r.end + base);
        let mut spans = opts.subsequent_indent.spans.clone();
        spans.extend(slice_spans(line, &bounds, &range));
        out.push(Line::from(spans).style(line.style));
    }
    out
}

pub fn text_contains_url_like(text: &str) -> bool {
    text.split_whitespace().any(is_url_like)
}

fn is_url_like(w: &str) -> bool {
    let t = w.trim_matches(|c: char| "()[]<>\"'`,;".contains(c));
    t.starts_with("http://") || t.starts_with("https://") || t.starts_with("file://")
}

pub fn line_contains_url_like(line: &Line<'_>) -> bool {
    let t: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    text_contains_url_like(&t)
}

/// True when a line has a URL token and also ordinary prose tokens.
pub fn line_has_mixed_url_and_non_url_tokens(line: &Line<'_>) -> bool {
    let t: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    let mut url = false;
    let mut other = false;
    for w in t.split_whitespace() {
        if is_url_like(w) {
            url = true;
        } else if w.chars().any(|c| c.is_alphanumeric()) {
            other = true;
        }
    }
    url && other
}

/// Wrap a history line; URL tokens are never split in the middle.
pub fn adaptive_wrap_line(line: &Line<'static>, opts: &WrapOpts) -> Vec<Line<'static>> {
    if line_contains_url_like(line) {
        let mut o = opts.clone();
        o.break_words = false;
        o.hyphen_split = false;
        o.uax14 = false;
        word_wrap_line(line, &o)
    } else {
        word_wrap_line(line, opts)
    }
}

/// Wrap plain text lines into styled rows with the same indent rules.
pub fn wrap_lines(lines: &[Line<'static>], opts: &WrapOpts) -> Vec<Line<'static>> {
    lines.iter().flat_map(|l| word_wrap_line(l, opts)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(text: &str, w: usize) -> Vec<String> {
        word_wrap_line(&Line::from(text.to_string()), &WrapOpts::new(w))
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn wraps_words_and_trims() {
        assert_eq!(rows("aaa bbb ccc", 7), vec!["aaa bbb", "ccc"]);
        assert_eq!(rows("aaa  bbb", 3), vec!["aaa", "bbb"]);
    }

    #[test]
    fn splits_after_hyphen_between_letters() {
        assert_eq!(rows("GPT-5.6 Sol", 4), vec!["GPT-", "5.6", "Sol"]);
    }

    #[test]
    fn breaks_long_words() {
        assert_eq!(rows("abcdefgh", 3), vec!["abc", "def", "gh"]);
    }

    #[test]
    fn indent_applies_to_continuation_rows() {
        let o = WrapOpts::new(8).subsequent_indent(Line::from("  "));
        let l = word_wrap_line(&Line::from("aaa bbb ccc"), &o);
        let s: Vec<String> = l
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(s, vec!["aaa bbb", "  ccc"]);
    }
}
