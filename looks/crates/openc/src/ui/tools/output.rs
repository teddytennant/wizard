//! Tool output as rows: ANSI colour kept, tabs and carriage returns handled, very long lines
//! cut after a few rows, binary and empty output named instead of drawn.

use ratatui::style::Style;
use ratatui::text::Span;
use tuikit::width::{display_width, wrap_spans, WrapMode};

use crate::ui::row::{sp, spaces, Cx, Row};

/// A single output line takes at most this many rows; the rest is counted, not drawn.
pub const MAX_LINE_ROWS: usize = 3;

pub struct Body {
    pub rows: Vec<Row>,
    /// Source lines left out because the cap was reached.
    pub more: usize,
}

/// Output the terminal could not show: NUL bytes, or mostly control and replacement
/// characters in the first couple of thousand.
pub fn is_binary(s: &str) -> bool {
    let mut n = 0usize;
    let mut bad = 0usize;
    for c in s.chars().take(2000) {
        n += 1;
        if c == '\0' {
            return true;
        }
        if c == '\u{fffd}' || (c.is_control() && !matches!(c, '\n' | '\t' | '\r' | '\u{1b}')) {
            bad += 1;
        }
    }
    n > 0 && bad * 10 > n
}

/// A line of a file read: `   12→code`.
pub fn numbered(l: &str) -> bool {
    l.split_once('→')
        .is_some_and(|(n, _)| !n.trim().is_empty() && n.trim().bytes().all(|b| b.is_ascii_digit()))
}

pub fn human_bytes(n: usize) -> String {
    match n {
        0..=1023 => format!("{n} B"),
        1024..=1_048_575 => format!("{:.1} KB", n as f64 / 1024.0),
        _ => format!("{:.1} MB", n as f64 / 1_048_576.0),
    }
}

fn plain_row(cx: &Cx, text: &str) -> Row {
    Row::new(vec![spaces(4), sp(text.to_string(), cx.p.s_faint())]).bg(cx.p.surface)
}

/// The rows of `out` at column 4, `w` cells wide, at most `cap` of them. `skip_first` drops
/// a first line that the summary row already says (`Exit code 2`).
/// What the CLI says for a command that printed nothing.
pub const NO_OUTPUT: &str = "(Bash completed with no output)";

/// `out` with that marker taken for what it means: nothing.
pub fn effective(out: &str) -> &str {
    if out.trim() == NO_OUTPUT {
        ""
    } else {
        out
    }
}

pub fn rows(out: &str, w: usize, cx: &Cx, cap: usize, skip_first: bool) -> Body {
    let p = cx.p;
    let out = effective(out);
    if out.trim().is_empty() {
        return Body {
            rows: vec![plain_row(cx, "no output")],
            more: 0,
        };
    }
    if is_binary(out) {
        return Body {
            rows: vec![plain_row(
                cx,
                &format!("binary output, {}", human_bytes(out.len())),
            )],
            more: 0,
        };
    }
    let base = p.s_dim();
    let mut lines = tuikit::ansi::to_lines_on(out.trim_end(), base, Some(p.surface));
    if skip_first && !lines.is_empty() {
        lines.remove(0);
    }
    let total = lines.len();
    let mut rows: Vec<Row> = Vec::new();
    let mut used = 0usize;
    for line in lines {
        if rows.len() >= cap {
            break;
        }
        used += 1;
        let spans: Vec<Span<'static>> = line.spans;
        let width: usize = spans.iter().map(|s| display_width(&s.content)).sum();
        if width <= w {
            let mut r = vec![spaces(4)];
            r.extend(spans);
            rows.push(Row::new(r).bg(p.surface));
            continue;
        }
        let wrapped = wrap_spans(&spans, w, WrapMode::Word);
        let n = wrapped.len();
        for (i, seg) in wrapped.into_iter().enumerate() {
            if i >= MAX_LINE_ROWS {
                break;
            }
            let mut r = vec![spaces(4)];
            if i == MAX_LINE_ROWS - 1 && n > MAX_LINE_ROWS {
                // The last drawn row says how much more there was.
                let left: usize = n - (MAX_LINE_ROWS - 1);
                r.push(sp(
                    format!("{} {} more chars", cx.g.wrap, left * w),
                    p.s_faint(),
                ));
            } else {
                r.extend(seg);
            }
            rows.push(Row::new(r).bg(p.surface));
        }
    }
    Body {
        rows,
        more: total.saturating_sub(used),
    }
}

/// Read results come as `   12→code`. Split the number off and colour the code by the file
/// type; anything that does not look like that falls back to [`rows`].
pub fn read_rows(out: &str, path: &str, w: usize, cx: &Cx, cap: usize) -> Body {
    let p = cx.p;
    let parsed: Vec<(usize, &str)> = out
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| {
            let (n, code) = l.split_once('→')?;
            Some((n.trim().parse().ok()?, code))
        })
        .collect();
    let total = out.lines().filter(|l| !l.trim().is_empty()).count();
    if parsed.is_empty() || parsed.len() * 10 < total * 8 || is_binary(out) {
        return rows(out, w, cx, cap, false);
    }
    let code: String = parsed.iter().map(|(_, c)| format!("{c}\n")).collect();
    let hl = tuikit::syntax::highlight(tuikit::syntax::find_syntax_for_path(path), &code, cx.theme);
    let num_w = parsed
        .iter()
        .map(|(n, _)| n.to_string().len())
        .max()
        .unwrap_or(1)
        .max(3);
    let code_w = w.saturating_sub(num_w + 1).max(8);
    let mut rows: Vec<Row> = Vec::new();
    let mut used = 0usize;
    for ((n, text), spans) in parsed.iter().zip(hl) {
        if rows.len() >= cap {
            break;
        }
        used += 1;
        let spans = if spans.is_empty() && !text.is_empty() {
            vec![sp(text.to_string(), p.s_text())]
        } else {
            spans
        };
        let wrapped = if spans.is_empty() {
            vec![Vec::new()]
        } else {
            crate::ui::wrapcode::wrap_code(&spans, code_w)
        };
        for (i, seg) in wrapped.into_iter().enumerate().take(MAX_LINE_ROWS) {
            let mut r = vec![spaces(4)];
            if i == 0 {
                r.push(sp(format!("{n:>num_w$} "), p.s_faint()));
            } else {
                r.push(spaces(num_w + 1 - 2));
                r.push(sp(cx.g.wrap, p.s_faint()));
                r.push(spaces(1));
            }
            r.extend(seg);
            rows.push(Row::new(r).bg(p.surface));
        }
    }
    Body {
        rows,
        more: parsed.len().saturating_sub(used),
    }
}

/// Search results: `path:line: text` with the path in `text` colour and the match dim.
pub fn search_rows(out: &str, w: usize, cx: &Cx, cap: usize) -> Body {
    let p = cx.p;
    if out.trim().is_empty() || is_binary(out) {
        return rows(out, w, cx, cap, false);
    }
    let lines: Vec<&str> = out.trim_end().lines().collect();
    let mut rows: Vec<Row> = Vec::new();
    let mut used = 0usize;
    for l in &lines {
        if rows.len() >= cap {
            break;
        }
        used += 1;
        let clean = tuikit::ansi::strip(l);
        let (head, tail) = match clean.find(':') {
            Some(i) if i > 0 && !clean[..i].contains(' ') => (&clean[..i], &clean[i..]),
            _ => ("", clean.as_str()),
        };
        let line = format!("{head}{tail}");
        let cut = tuikit::width::truncate(&line, w);
        let (a, b) = if head.is_empty() {
            (String::new(), cut)
        } else {
            let hw = display_width(head).min(w);
            let a = tuikit::width::truncate(head, hw);
            let b: String = cut.chars().skip(a.chars().count()).collect();
            (a, b)
        };
        let mut r = vec![spaces(4)];
        if !a.is_empty() {
            r.push(sp(a, Style::new().fg(p.text)));
        }
        r.push(sp(b, p.s_dim()));
        rows.push(Row::new(r).bg(p.surface));
    }
    Body {
        rows,
        more: lines.len().saturating_sub(used),
    }
}

/// A failed shell call's output without the `Exit code N` line the summary row already says.
pub fn without_exit_line(out: &str) -> &str {
    match out.split_once('\n') {
        Some((first, rest)) if first.starts_with("Exit code ") => rest,
        None if out.starts_with("Exit code ") => "",
        _ => out,
    }
}

/// The last `n` non-empty lines of running output, colour stripped, one row each.
pub fn tail(out: &str, n: usize, w: usize, cx: &Cx) -> Vec<Row> {
    if is_binary(out) {
        return Vec::new();
    }
    let clean = tuikit::ansi::strip(out);
    let lines: Vec<&str> = clean.lines().filter(|l| !l.trim().is_empty()).collect();
    lines
        .iter()
        .skip(lines.len().saturating_sub(n))
        .map(|l| {
            Row::new(vec![
                spaces(4),
                sp(tuikit::width::truncate(l.trim_end(), w), cx.p.s_faint()),
            ])
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::palette::{Depth, Kind, Palette, UNICODE};
    use std::time::Instant;

    fn with_cx<R>(f: impl FnOnce(&Cx) -> R) -> R {
        let p = Palette::new(Kind::Hearth, Depth::True);
        let t = p.theme();
        let cx = Cx {
            p: &p,
            theme: &t,
            g: &UNICODE,
            width: 60,
            detail: false,
            now: Instant::now(),
            spin: 0,
        };
        f(&cx)
    }

    #[test]
    fn binary_and_empty_are_named() {
        assert!(is_binary("\u{fffd}PNG\r\n\u{1a}\n\0\0"));
        assert!(!is_binary("plain text\twith a tab\nand lines\n"));
        with_cx(|cx| {
            let b = rows("", 50, cx, 40, false);
            assert_eq!(b.rows[0].text().trim(), "no output");
            let b = rows("\0\0\0\0", 50, cx, 40, false);
            assert!(b.rows[0].text().contains("binary output, 4 B"));
        });
    }

    #[test]
    fn ansi_colour_survives_and_escape_bytes_do_not_reach_the_text() {
        with_cx(|cx| {
            let b = rows("\u{1b}[1;34msrc\u{1b}[0m\nCargo.toml", 50, cx, 40, false);
            assert_eq!(b.rows[0].text().trim(), "src");
            assert!(!b.rows[0].text().contains('\u{1b}'));
            let blue = b.rows[0].spans.iter().find(|s| s.content == "src").unwrap();
            assert_ne!(blue.style.fg, cx.p.s_dim().fg, "the colour came through");
        });
    }

    #[test]
    fn a_very_long_line_takes_three_rows_and_says_how_much_was_cut() {
        with_cx(|cx| {
            let long = "x".repeat(5000);
            let b = rows(&format!("{long}\nshort"), 40, cx, 40, false);
            let t: Vec<String> = b.rows.iter().map(Row::text).collect();
            assert_eq!(t.len(), 4, "{t:?}");
            assert!(t[2].contains("more chars"), "{t:?}");
            assert_eq!(t[3].trim(), "short");
        });
    }

    #[test]
    fn cap_counts_the_lines_it_left_out() {
        with_cx(|cx| {
            let out: String = (0..100).map(|i| format!("line {i}\n")).collect();
            let b = rows(&out, 40, cx, 40, false);
            assert_eq!((b.rows.len(), b.more), (40, 60));
        });
    }

    #[test]
    fn read_output_splits_the_line_numbers_off() {
        with_cx(|cx| {
            let b = read_rows("     1→fn main() {\n     2→}\n", "a.rs", 60, cx, 40);
            assert_eq!(b.rows[0].text().trim_start(), "1 fn main() {");
        });
    }
}
