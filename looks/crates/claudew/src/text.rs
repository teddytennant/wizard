//! Styled rows: build them from spans, wrap them at a width, draw them into a buffer.

use ratatui::buffer::Buffer;
use ratatui::style::Style;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub type Span = (String, Style);

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Row(pub Vec<Span>);

impl Row {
    pub fn new() -> Row {
        Row(Vec::new())
    }

    pub fn of(s: impl Into<String>, style: Style) -> Row {
        Row(vec![(s.into(), style)])
    }

    pub fn push(&mut self, s: impl Into<String>, style: Style) -> &mut Row {
        let s = s.into();
        if !s.is_empty() {
            self.0.push((s, style));
        }
        self
    }

    pub fn with(mut self, s: impl Into<String>, style: Style) -> Row {
        self.push(s, style);
        self
    }

    pub fn extend(&mut self, other: Row) -> &mut Row {
        self.0.extend(other.0);
        self
    }

    pub fn width(&self) -> usize {
        self.0.iter().map(|(s, _)| width(s)).sum()
    }

    pub fn text(&self) -> String {
        self.0.iter().map(|(s, _)| s.as_str()).collect()
    }
}

pub fn width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Draw `s` at `x`, `y`, clipped to the buffer. Returns the column after the last cell.
pub fn put(buf: &mut Buffer, x: u16, y: u16, s: &str, style: Style) -> u16 {
    let area = buf.area;
    let mut x = x;
    if y < area.y || y >= area.bottom() {
        return x;
    }
    for g in s.graphemes(true) {
        let w = width(g) as u16;
        if w == 0 {
            continue;
        }
        if x + w > area.right() {
            break;
        }
        if let Some(cell) = buf.cell_mut((x, y)) {
            cell.set_symbol(g);
            cell.set_style(style);
        }
        for i in 1..w {
            if let Some(cell) = buf.cell_mut((x + i, y)) {
                cell.set_symbol("");
                cell.set_style(style);
            }
        }
        x += w;
    }
    x
}

pub fn blit(buf: &mut Buffer, x: u16, y: u16, row: &Row) -> u16 {
    let mut x = x;
    for (s, style) in &row.0 {
        x = put(buf, x, y, s, *style);
    }
    x
}

/// `s` cut to `max` columns, the last one an ellipsis when something was dropped.
pub fn truncate(s: &str, max: usize) -> String {
    if width(s) <= max {
        return s.to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    for g in s.graphemes(true) {
        let gw = width(g);
        if w + gw + 1 > max {
            break;
        }
        out.push_str(g);
        w += gw;
    }
    out.push('…');
    out
}

/// Greedy word wrap that keeps each grapheme's style. A break swallows the spaces around it;
/// a word wider than the row is cut. An empty input gives one empty row.
pub fn wrap(spans: &[Span], max: usize) -> Vec<Row> {
    let max = max.max(1);
    let mut cells: Vec<(&str, Style)> = Vec::new();
    for (s, style) in spans {
        for g in s.graphemes(true) {
            cells.push((g, *style));
        }
    }
    let mut rows: Vec<Vec<(&str, Style)>> = Vec::new();
    let mut line: Vec<(&str, Style)> = Vec::new();
    let mut line_w = 0;
    let mut i = 0;
    while i < cells.len() {
        if cells[i].0 == "\n" {
            rows.push(std::mem::take(&mut line));
            line_w = 0;
            i += 1;
            continue;
        }
        let space = cells[i].0 == " ";
        let mut j = i;
        let mut w = 0;
        while j < cells.len() && cells[j].0 != "\n" && (cells[j].0 == " ") == space {
            w += width(cells[j].0);
            j += 1;
        }
        if space {
            if line_w + w <= max {
                line.extend_from_slice(&cells[i..j]);
                line_w += w;
            } else {
                rows.push(std::mem::take(&mut line));
                line_w = 0;
            }
        } else if line_w + w <= max {
            line.extend_from_slice(&cells[i..j]);
            line_w += w;
        } else {
            // a word wider than a whole row is cut where it stands; a shorter one moves down
            if line_w > 0 && w <= max {
                while line.last().is_some_and(|c| c.0 == " ") {
                    line.pop();
                }
                rows.push(std::mem::take(&mut line));
                line_w = 0;
            }
            for c in &cells[i..j] {
                let cw = width(c.0);
                if line_w + cw > max {
                    rows.push(std::mem::take(&mut line));
                    line_w = 0;
                }
                line.push(*c);
                line_w += cw;
            }
        }
        i = j;
    }
    rows.push(line);
    rows.into_iter()
        .map(|cells| {
            let mut row = Row::new();
            for (g, style) in cells {
                match row.0.last_mut() {
                    Some((s, st)) if *st == style => s.push_str(g),
                    _ => row.0.push((g.to_string(), style)),
                }
            }
            row
        })
        .collect()
}

/// [`wrap`] with a first row of `first` columns and the rest of `rest`.
pub fn wrap_first(spans: &[Span], first: usize, rest: usize) -> Vec<Row> {
    let mut rows = wrap(spans, first);
    if rows.len() < 2 || first == rest {
        return rows;
    }
    let head = rows.remove(0);
    let mut skip = head.text().graphemes(true).count();
    let mut left: Vec<Span> = Vec::new();
    for (s, style) in spans {
        let n = s.graphemes(true).count();
        if skip >= n {
            skip -= n;
            continue;
        }
        left.push((s.graphemes(true).skip(skip).collect(), *style));
        skip = 0;
    }
    while let Some((s, _)) = left.first_mut() {
        let t = s.trim_start().to_string();
        if t.is_empty() {
            left.remove(0);
        } else {
            *s = t;
            break;
        }
    }
    let mut out = vec![head];
    out.extend(wrap(&left, rest));
    out
}

pub fn wrap_str(s: &str, style: Style, max: usize) -> Vec<Row> {
    wrap(&[(s.to_string(), style)], max)
}

/// `n` with thousands separators, as Claude Code prints token counts.
pub fn commas(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_on_words_and_drops_the_break_space() {
        let rows = wrap_str("alpha beta gamma", Style::new(), 11);
        let t: Vec<String> = rows.iter().map(Row::text).collect();
        assert_eq!(t, ["alpha beta", "gamma"]);
    }

    #[test]
    fn cuts_a_word_longer_than_the_row() {
        let rows = wrap_str("abcdefgh", Style::new(), 3);
        let t: Vec<String> = rows.iter().map(Row::text).collect();
        assert_eq!(t, ["abc", "def", "gh"]);
    }

    #[test]
    fn keeps_blank_lines_and_truncates() {
        let rows = wrap_str("a\n\nb", Style::new(), 10);
        assert_eq!(rows.len(), 3);
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(commas(1234567), "1,234,567");
    }
}
