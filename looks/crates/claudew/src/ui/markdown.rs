//! Markdown in replies, the way Claude Code prints it: tables drawn with box characters,
//! `code` in blue with the backticks dropped, links in bright blue that the terminal can open,
//! fenced code in the terminal's own colours.

use ratatui::style::{Color, Modifier, Style};

use crate::text::{width, Row, Span};
use crate::theme::Theme;
use crate::ui::diff::Highlighter;

/// The colour of a link's text. The draw pass finds runs of it to wrap them in OSC 8.
pub const LINK: Color = Color::Indexed(12);
/// Fenced code colours by highlighter slot: plain, keyword, name, string, number, comment,
/// macro.
const FENCE: [Option<Color>; 7] = [
    None,
    Some(Color::Indexed(4)),
    Some(Color::Indexed(3)),
    Some(Color::Indexed(1)),
    Some(Color::Indexed(5)),
    Some(Color::Indexed(2)),
    Some(Color::Indexed(6)),
];

/// Inline markdown as spans: `code`, **bold**, *italic*, [text](url). Links found are added
/// to `links` as (text, url). Anything unbalanced is left as typed.
pub fn inline(th: &Theme, text: &str, base: Style, links: &mut Vec<(String, String)>) -> Vec<Span> {
    inline_with(th, text, base, Some(links))
}

/// [`inline`], leaving `[text](url)` as typed when `links` is `None`, as in what you sent.
pub fn inline_with(
    th: &Theme,
    text: &str,
    base: Style,
    mut links: Option<&mut Vec<(String, String)>>,
) -> Vec<Span> {
    let chars: Vec<char> = text.chars().collect();
    let mut out: Vec<Span> = Vec::new();
    let mut plain = String::new();
    let find = |from: usize, pat: &[char]| -> Option<usize> {
        if pat.is_empty() || chars.len() < pat.len() {
            return None;
        }
        (from..=chars.len() - pat.len()).find(|&k| chars[k..k + pat.len()] == *pat)
    };
    let flush = |plain: &mut String, out: &mut Vec<Span>| {
        if !plain.is_empty() {
            out.push((std::mem::take(plain), base));
        }
    };
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '`' {
            if let Some(end) = find(i + 1, &['`']).filter(|&e| e > i + 1) {
                flush(&mut plain, &mut out);
                out.push((chars[i + 1..end].iter().collect(), base.fg(th.suggestion)));
                i = end + 1;
                continue;
            }
        }
        if c == '[' && links.is_some() {
            if let Some(close) = find(i + 1, &[']']) {
                if chars.get(close + 1) == Some(&'(') {
                    if let Some(end) = find(close + 2, &[')']) {
                        let label: String = chars[i + 1..close].iter().collect();
                        let url: String = chars[close + 2..end].iter().collect();
                        if !label.is_empty() && !url.contains(' ') {
                            flush(&mut plain, &mut out);
                            if hyperlinks() {
                                if let Some(l) = links.as_deref_mut() {
                                    l.push((label.clone(), url));
                                }
                                out.push((label, base.fg(LINK)));
                            } else {
                                out.push((format!("{label} ({url})"), base));
                            }
                            i = end + 1;
                            continue;
                        }
                    }
                }
            }
        }
        let marker: &[char] = if c == '*' && chars.get(i + 1) == Some(&'*') {
            &['*', '*']
        } else if c == '*' && chars.get(i + 1).is_some_and(|n| !n.is_whitespace()) {
            &['*']
        } else {
            &[]
        };
        if !marker.is_empty() {
            let from = i + marker.len();
            if let Some(end) = find(from, marker).filter(|&e| e > from) {
                let add = if marker.len() == 2 {
                    Modifier::BOLD
                } else {
                    Modifier::ITALIC
                };
                let inner: String = chars[from..end].iter().collect();
                flush(&mut plain, &mut out);
                out.extend(inline_with(
                    th,
                    &inner,
                    base.add_modifier(add),
                    links.as_deref_mut(),
                ));
                i = end + marker.len();
                continue;
            }
        }
        plain.push(c);
        i += 1;
    }
    flush(&mut plain, &mut out);
    out
}

/// Whether the terminal opens OSC 8 links, judged from the environment the way Claude Code
/// judges it. Without them a link is printed as `text (url)`.
pub fn hyperlinks() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let var = |k: &str| std::env::var(k).unwrap_or_default();
        match var("FORCE_HYPERLINK").as_str() {
            "0" | "false" => return false,
            "" => {}
            _ => return true,
        }
        if !var("WT_SESSION").is_empty() || !var("KONSOLE_VERSION").is_empty() {
            return true;
        }
        if var("VTE_VERSION").parse::<u32>().is_ok_and(|v| v >= 5000) {
            return true;
        }
        // TERM_PROGRAM with its version, as Claude Code reads it: tmux links from 3.4
        let version: Vec<u32> = var("TERM_PROGRAM_VERSION")
            .split(['.', '-'])
            .map(|p| {
                p.trim_end_matches(|c: char| c.is_ascii_alphabetic())
                    .parse()
                    .unwrap_or(0)
            })
            .collect();
        let at_least = |want: &[u32]| {
            let have = |i: usize| version.get(i).copied().unwrap_or(0);
            want.iter()
                .enumerate()
                .find(|(i, w)| have(*i) != **w)
                .is_none_or(|(i, w)| have(i) > *w)
        };
        match var("TERM_PROGRAM").as_str() {
            "tmux" => return at_least(&[3, 4]),
            "iTerm.app" => return at_least(&[3, 1]),
            "WezTerm" => return at_least(&[20200620]),
            "vscode" => return at_least(&[1, 72]),
            "ghostty" => return true,
            _ => {}
        }
        let term = var("TERM");
        ["kitty", "foot", "ghostty", "alacritty", "wezterm"]
            .iter()
            .any(|t| term.contains(t))
    })
}

/// A line of markdown source as the reference colours it inside a fence: `**strong**` bold,
/// a link's text in red and its target underlined, everything else plain.
fn markdown_source(line: &str) -> Row {
    let mut row = Row::new();
    let mut rest = line;
    while !rest.is_empty() {
        let strong = rest.find("**").and_then(|a| {
            let end = rest[a + 2..].find("**")? + a + 4;
            Some((a, end, 0))
        });
        let link = rest.find('[').and_then(|a| {
            let close = rest[a..].find("](")? + a;
            let end = rest[close..].find(')')? + close + 1;
            Some((a, end, close))
        });
        let next = match (strong, link) {
            (Some(s), Some(l)) => Some(if s.0 <= l.0 { s } else { l }),
            (s, l) => s.or(l),
        };
        let Some((at, end, close)) = next else {
            row.push(rest, Style::new());
            break;
        };
        row.push(&rest[..at], Style::new());
        if close == 0 {
            row.push(&rest[at..end], Style::new().add_modifier(Modifier::BOLD));
        } else {
            row.push("[", Style::new());
            row.push(
                &rest[at + 1..close],
                Style::new().fg(ratatui::style::Color::Indexed(1)),
            );
            row.push("](", Style::new());
            row.push(
                &rest[close + 2..end - 1],
                Style::new().add_modifier(Modifier::UNDERLINED),
            );
            row.push(")", Style::new());
        }
        rest = &rest[end..];
    }
    row
}

fn table_cells(line: &str) -> Vec<String> {
    let t = line.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = t.strip_suffix('|').unwrap_or(t);
    t.split('|').map(|c| c.trim().to_string()).collect()
}

fn is_rule(line: &str) -> bool {
    let cells = table_cells(line);
    !cells.is_empty()
        && cells
            .iter()
            .all(|c| !c.is_empty() && c.chars().all(|ch| matches!(ch, '-' | ':' | ' ')))
}

/// A pipe table drawn with box characters, or `None` when it does not fit `max` columns.
fn table(
    th: &Theme,
    lines: &[&str],
    max: usize,
    links: &mut Vec<(String, String)>,
) -> Option<Vec<Row>> {
    let rows: Vec<Vec<Vec<Span>>> = lines
        .iter()
        .filter(|l| !is_rule(l))
        .map(|l| {
            table_cells(l)
                .iter()
                .map(|c| inline(th, c, Style::new(), links))
                .collect()
        })
        .collect();
    let cols = rows.iter().map(Vec::len).max()?;
    let span_w = |c: &Vec<Span>| c.iter().map(|(s, _)| width(s)).sum::<usize>();
    let widths: Vec<usize> = (0..cols)
        .map(|i| {
            rows.iter()
                .filter_map(|r| r.get(i))
                .map(span_w)
                .max()
                .unwrap_or(0)
        })
        .collect();
    if widths.iter().map(|w| w + 3).sum::<usize>() + 1 > max {
        return None;
    }
    let rule = |l: &str, m: &str, r: &str| -> Row {
        let parts: Vec<String> = widths.iter().map(|w| "─".repeat(w + 2)).collect();
        Row::of(format!("{l}{}{r}", parts.join(m)), Style::new())
    };
    let mut out = vec![rule("┌", "┬", "┐")];
    for (n, r) in rows.iter().enumerate() {
        if n > 0 {
            out.push(rule("├", "┼", "┤"));
        }
        let mut row = Row::of("│", Style::new());
        for (i, w) in widths.iter().enumerate() {
            row.push(" ", Style::new());
            let cell = r.get(i).cloned().unwrap_or_default();
            let cw = span_w(&cell);
            row.0.extend(cell);
            row.push(format!("{} │", " ".repeat(w - cw)), Style::new());
        }
        out.push(row);
    }
    out.push(rule("└", "┴", "┘"));
    Some(out)
}

/// The body of a reply as rows `max` columns wide, without the bullet.
pub fn render(th: &Theme, text: &str, max: usize, links: &mut Vec<(String, String)>) -> Vec<Row> {
    let lines: Vec<&str> = text.trim().lines().collect();
    let mut out: Vec<Row> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let t = lines[i].trim_end();
        if let Some(lang) = t.trim_start().strip_prefix("```") {
            // a fenced markdown block is shown as written, not highlighted
            let as_code = matches!(lang.trim(), "markdown" | "md");
            let mut hl = Highlighter::new(lang.trim());
            let known = hl.known() && !as_code;
            i += 1;
            // only a bare fence closes the block; ```rust inside it is content
            while i < lines.len() && lines[i].trim() != "```" {
                let mut row = Row::new();
                if known {
                    for (s, slot) in hl.line(lines[i]) {
                        let st = FENCE[slot].map_or(Style::new(), |c| Style::new().fg(c));
                        row.push(s, st);
                    }
                } else if as_code {
                    // shown as written, with the reference's light markdown colouring
                    row.extend(markdown_source(lines[i]));
                } else {
                    row.push(lines[i], Style::new());
                }
                out.push(row);
                i += 1;
            }
            i += 1;
            continue;
        }
        if t.trim_start().starts_with('|') {
            let start = i;
            while i < lines.len() && lines[i].trim_start().starts_with('|') {
                i += 1;
            }
            match table(th, &lines[start..i], max, links) {
                Some(rows) => out.extend(rows),
                None => out.extend(lines[start..i].iter().map(|l| Row::of(*l, Style::new()))),
            }
            continue;
        }
        let (lead, rest, base) = if let Some(h) = t.strip_prefix("# ") {
            let st =
                Style::new().add_modifier(Modifier::BOLD | Modifier::ITALIC | Modifier::UNDERLINED);
            ("", h, st)
        } else if let Some(h) = t.strip_prefix("## ").or_else(|| t.strip_prefix("### ")) {
            ("", h, Style::new().add_modifier(Modifier::BOLD))
        } else if let Some(q) = t.strip_prefix("> ") {
            ("▎ ", q, th.dim().add_modifier(Modifier::ITALIC))
        } else {
            ("", t, Style::new())
        };
        let hang = list_indent(rest);
        // a list item's first row has the full width, its marker included; the rows under it
        // hang past the marker
        for (k, r) in crate::text::wrap_first(
            &inline(th, rest, base, links),
            max.max(1),
            max.saturating_sub(hang).max(1),
        )
        .into_iter()
        .enumerate()
        {
            let pad = if k == 0 {
                String::new()
            } else {
                " ".repeat(hang)
            };
            let mut row = Row::of(lead, th.dim()).with(pad, Style::new());
            row.extend(r);
            out.push(row);
        }
        i += 1;
    }
    out
}

/// How far a list item's wrapped rows hang under its marker.
fn list_indent(line: &str) -> usize {
    let t = line.trim_start();
    let lead = line.len() - t.len();
    if t.starts_with("- ") || t.starts_with("* ") {
        return lead + 2;
    }
    let digits = t.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 && t[digits..].starts_with(". ") {
        return lead + digits + 2;
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(rows: &[Row]) -> Vec<String> {
        rows.iter().map(|r| r.text()).collect()
    }

    #[test]
    fn a_pipe_table_is_drawn_with_box_characters() {
        let mut links = Vec::new();
        let md = "|file|lines|\n|---|---|\n|a.rs|3|\n|b.rs|120|";
        let rows = render(&Theme::dark(), md, 117, &mut links);
        assert_eq!(
            texts(&rows),
            [
                "┌──────┬───────┐",
                "│ file │ lines │",
                "├──────┼───────┤",
                "│ a.rs │ 3     │",
                "├──────┼───────┤",
                "│ b.rs │ 120   │",
                "└──────┴───────┘",
            ]
        );
    }

    #[test]
    fn bold_code_drops_the_marks_and_links_are_recorded() {
        let th = Theme::dark();
        let mut links = Vec::new();
        let spans = inline(
            &th,
            "**`src/`** and [Docs](https://example.com/docs)",
            Style::new(),
            &mut links,
        );
        assert_eq!(spans[0].0, "src/");
        assert_eq!(
            spans[0].1,
            Style::new().add_modifier(Modifier::BOLD).fg(th.suggestion)
        );
        let last = &spans.last().unwrap().0;
        if hyperlinks() {
            assert_eq!(last, "Docs");
            assert_eq!(
                links,
                [("Docs".to_string(), "https://example.com/docs".to_string())]
            );
        } else {
            assert_eq!(last, "Docs (https://example.com/docs)");
        }
    }

    #[test]
    fn fenced_rust_is_coloured() {
        let mut links = Vec::new();
        let rows = render(&Theme::dark(), "```rust\nfn main() {}\n```", 80, &mut links);
        assert_eq!(
            rows[0].0[0],
            ("fn".to_string(), Style::new().fg(Color::Indexed(4)))
        );
    }
}
