//! Text helpers for the tool renderers: output cleaning, argument access, the Pi `Text` wrap,
//! hint rows and durations.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use serde_json::Value;
use tuikit::width::{wrap_spans, WrapMode};

use crate::theme::Tok;
use crate::ui::{span, Cx, Lines};

pub const EXPAND_KEY: &str = "ctrl+o";

/// ANSI stripped, `\r` dropped, control characters other than `\n` dropped, tabs as three
/// spaces (pi-tui's `Text` does the same).
pub fn clean(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\x1b' => match it.peek() {
                Some('[') => {
                    it.next();
                    for d in it.by_ref() {
                        if ('@'..='~').contains(&d) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    for d in it.by_ref() {
                        if d == '\x07' {
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\r' => {}
            '\t' => out.push_str("   "),
            c if c == '\n' || !c.is_control() => out.push(c),
            _ => {}
        }
    }
    out
}

pub fn arg<'a>(input: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    keys.iter()
        .find_map(|k| input.get(*k).filter(|v| !v.is_null()))
}

pub fn arg_str(input: &Value, keys: &[&str]) -> Option<String> {
    arg(input, keys).and_then(Value::as_str).map(String::from)
}

pub fn arg_u64(input: &Value, keys: &[&str]) -> Option<u64> {
    arg(input, keys).and_then(Value::as_u64)
}

/// `~` for the home directory, as Pi's `shortenPath`.
pub fn shorten_path(p: &str, home: &str) -> String {
    if !home.is_empty() && p.starts_with(home) {
        format!("~{}", &p[home.len()..])
    } else {
        p.to_string()
    }
}

/// Wrap one logical row of spans at `width`; an empty row stays one blank row.
pub fn wrap_row(spans: Vec<Span<'static>>, width: u16) -> Lines {
    if spans.iter().all(|s| s.content.is_empty()) {
        return vec![Line::from("")];
    }
    wrap_spans(&spans, width.max(1) as usize, WrapMode::Word)
        .into_iter()
        .map(Line::from)
        .collect()
}

/// Wrap each `\n`-separated line of `text` in one style.
pub fn wrap_plain(text: &str, width: u16, style: Style) -> Lines {
    text.split('\n')
        .flat_map(|l| wrap_row(vec![span(l.to_string(), style)], width))
        .collect()
}

/// `... (N more lines, ctrl+o to expand)`: muted, the key in `dim`. `total` adds `, T total`.
pub fn more_hint(cx: &Cx, remaining: usize, total: Option<usize>, tail: &str) -> Line<'static> {
    let th = cx.th();
    let muted = th.fg(Tok::Muted);
    let head = match total {
        Some(t) => format!("... ({remaining} more lines, {t} total,"),
        None => format!("... ({remaining} {tail},"),
    };
    Line::from(vec![
        span(head, muted),
        Span::raw(" "),
        span(EXPAND_KEY, th.fg(Tok::Dim)),
        span(" to expand", muted),
        span(")", muted),
    ])
}

/// Pi's `formatDuration`: `0.0s`, `1m 5s`, `1h 2m 3s`.
pub fn duration(d: std::time::Duration) -> String {
    let secs = d.as_secs_f64();
    if secs < 60.0 {
        return format!("{secs:.1}s");
    }
    let total = secs.floor() as u64;
    let (m, r) = (total / 60, total % 60);
    if m < 60 {
        format!("{m}m {r}s")
    } else {
        format!("{}h {}m {}s", m / 60, m % 60, r)
    }
}

/// Trailing empty entries dropped.
pub fn trim_trailing_empty<T: AsRef<str>>(mut v: Vec<T>) -> Vec<T> {
    while v.last().is_some_and(|l| l.as_ref().is_empty()) {
        v.pop();
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_strips_escapes_and_tabs() {
        assert_eq!(clean("\x1b[31mred\x1b[0m\r\na\tb"), "red\na   b");
    }

    #[test]
    fn durations() {
        assert_eq!(duration(std::time::Duration::from_millis(40)), "0.0s");
        assert_eq!(duration(std::time::Duration::from_secs(65)), "1m 5s");
        assert_eq!(duration(std::time::Duration::from_secs(3723)), "1h 2m 3s");
    }
}
