//! Transcript blocks the selector commands print: `/session` (spec 11.10), `/hotkeys` (11.11) and
//! `/changelog`. Rows come back ready to sit in the transcript; `session_info` has no padding of
//! its own (the caller's block adds the blank row and the one column), the other two draw their
//! own rules and padding.

use agent_core::Usage;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use tuikit::width::{spans_width, wrap_spans, WrapMode};

use crate::keys::{Action, Keymap};
use crate::theme::Tok;
use crate::ui::markdown_theme;
use crate::ui::messages::group_digits;
use crate::ui::{blank, indent, rule, span, Cx, Lines};

/// What `/session` counts.
#[derive(Default, Clone, Debug)]
pub struct SessionStats<'a> {
    pub name: Option<&'a str>,
    pub id: &'a str,
    pub path: String,
    pub users: usize,
    pub assistants: usize,
    pub tool_calls: usize,
    pub tool_results: usize,
    pub usage: Usage,
}

pub fn session_info(cx: &Cx, s: &SessionStats) -> Lines {
    let th = cx.th();
    let dim = th.fg(Tok::Dim);
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let kv = |k: &str, v: String| {
        Line::from(vec![span(format!("{k}:"), dim), Span::raw(format!(" {v}"))])
    };
    let head = |t: &str| Line::from(span(t.to_string(), bold));
    let n = |v: usize| group_digits(v as u64);
    let mut out: Lines = vec![head("Session Info"), blank()];
    if let Some(name) = s.name {
        out.push(kv("Name", name.to_string()));
    }
    out.push(kv("File", s.path.clone()));
    out.push(kv("ID", s.id.to_string()));
    out.push(blank());
    out.push(head("Messages"));
    out.push(kv("Total", n(s.users + s.assistants + s.tool_results)));
    out.push(kv("User", n(s.users)));
    out.push(kv("Assistant", n(s.assistants)));
    out.push(kv(
        "Tools",
        format!("{} calls, {} results", n(s.tool_calls), n(s.tool_results)),
    ));
    let u = &s.usage;
    if u.input_tokens + u.output_tokens > 0 {
        out.push(blank());
        out.push(head("Tokens"));
        out.push(kv("Input", group_digits(u.input_tokens)));
        if u.cached_tokens > 0 {
            let pct = u.cached_tokens as f64 * 100.0 / u.input_tokens.max(1) as f64;
            out.push(Line::from(vec![
                Span::raw("  "),
                span("Cached:", dim),
                Span::raw(format!(" {} ", group_digits(u.cached_tokens))),
                span(format!("({pct:.1}%)"), dim),
            ]));
            out.push(Line::from(vec![
                Span::raw("  "),
                span("Uncached:", dim),
                Span::raw(format!(
                    " {}",
                    group_digits(u.input_tokens.saturating_sub(u.cached_tokens))
                )),
            ]));
        }
        out.push(kv("Output", group_digits(u.output_tokens)));
        out.push(kv("Total", group_digits(u.input_tokens + u.output_tokens)));
    }
    if let Some(c) = u.cost_usd.filter(|c| *c > 0.0) {
        out.push(blank());
        out.push(head("Cost"));
        out.push(kv("Total", format!("${c:.3}")));
    }
    out
}

fn title_block(cx: &Cx, title: &str, body: Lines) -> Lines {
    let th = cx.th();
    let r = rule(cx.width, th.fg(Tok::Border));
    let mut out = vec![blank(), r.clone()];
    out.push(indent(
        Line::from(span(title.to_string(), th.bold(Tok::Accent))),
        1,
    ));
    out.push(blank());
    out.extend(body);
    out.push(r);
    out
}

/// `/changelog`: the `What's New` block with piw's own entries.
pub fn changelog(cx: &Cx) -> Lines {
    let th = cx.th();
    let md = markdown_theme::render(
        "## [0.1.0] - 2026-10-05\n\n### Added\n\n- Pi 1.0.3's terminal UI on the wizard backend: header, editor, messages, tool blocks, footer, slash commands and selectors.\n- `--print`, `--continue`, `--resume`, `--session`, `--model` and `--thinking`.",
        cx.width.saturating_sub(2).max(1),
        th,
        Style::default(),
    );
    let mut body = vec![blank()];
    body.extend(md.into_iter().map(|l| indent(l, 1)));
    body.push(blank());
    title_block(cx, "What's New", body)
}

fn cap(c: &crate::keys::Chord) -> String {
    c.text(true)
}

fn keys_of(km: &Keymap, a: Action) -> String {
    km.chords(a).iter().map(cap).collect::<Vec<_>>().join("/")
}

/// One cell: key groups in `mdCode` joined by ` / `, or plain text.
enum Cell {
    Keys(Vec<String>),
    Text(String),
}

fn rows(km: &Keymap) -> Vec<(&'static str, Vec<(Cell, &'static str)>)> {
    let k = |s: &[&str]| Cell::Keys(s.iter().map(|x| x.to_string()).collect());
    let a = |x: Action| Cell::Keys(vec![keys_of(km, x)]);
    vec![
        (
            "Navigation",
            vec![
                (
                    k(&["Up", "Down", "Left/Ctrl+B", "Right/Ctrl+F"]),
                    "Move cursor / browse history",
                ),
                (
                    k(&["Alt+Left/Ctrl+Left/Alt+B", "Alt+Right/Ctrl+Right/Alt+F"]),
                    "Move by word",
                ),
                (k(&["Home/Ctrl+A"]), "Start of line"),
                (k(&["End/Ctrl+E"]), "End of line"),
                (
                    Cell::Keys(vec![
                        keys_of(km, Action::PageUp),
                        keys_of(km, Action::PageDown),
                    ]),
                    "Scroll by page",
                ),
            ],
        ),
        (
            "Editing",
            vec![
                (a(Action::Submit), "Send message"),
                (a(Action::NewLine), "New line"),
                (k(&["Ctrl+W/Alt+Backspace"]), "Delete word backwards"),
                (k(&["Alt+D/Alt+Delete"]), "Delete word forwards"),
                (k(&["Ctrl+U"]), "Delete to start of line"),
                (k(&["Ctrl+K"]), "Delete to end of line"),
                (k(&["Ctrl+Y"]), "Paste the most-recently-deleted text"),
                (k(&["Ctrl+-"]), "Undo"),
            ],
        ),
        (
            "Other",
            vec![
                (a(Action::Tab), "Path completion / accept autocomplete"),
                (
                    a(Action::Interrupt),
                    "Cancel autocomplete / abort streaming",
                ),
                (a(Action::Clear), "Clear editor (first) / exit (second)"),
                (a(Action::Exit), "Exit (when editor is empty)"),
                (a(Action::Suspend), "Suspend to background"),
                (a(Action::ThinkingCycle), "Cycle thinking level"),
                (
                    Cell::Keys(vec![
                        keys_of(km, Action::ModelCycleForward),
                        "Shift+Ctrl+P".into(),
                    ]),
                    "Cycle models",
                ),
                (a(Action::ModelSelect), "Open model selector"),
                (a(Action::ToolsExpand), "Toggle tool output expansion"),
                (
                    a(Action::ThinkingToggle),
                    "Toggle thinking block visibility",
                ),
                (a(Action::ExternalEditor), "Edit message in external editor"),
                (a(Action::MessageCopy), "Copy last assistant message"),
                (a(Action::FollowUp), "Queue follow-up message"),
                (a(Action::Dequeue), "Restore queued messages"),
                (a(Action::PasteImage), "Paste text from clipboard"),
                (Cell::Text("/".into()), "Slash commands"),
                (Cell::Text("!".into()), "Run bash command"),
                (
                    Cell::Text("!!".into()),
                    "Run bash command (excluded from context)",
                ),
            ],
        ),
    ]
}

fn cell_spans(c: &Cell, cx: &Cx) -> Vec<Span<'static>> {
    match c {
        Cell::Text(t) => vec![span(t.clone(), cx.th().fg(Tok::MdCode))],
        Cell::Keys(g) => {
            let mut v = Vec::new();
            for (i, k) in g.iter().enumerate() {
                if i > 0 {
                    v.push(Span::raw(" / "));
                }
                v.push(span(k.clone(), cx.th().fg(Tok::MdCode)));
            }
            v
        }
    }
}

/// A two-column grid in the markdown table style: box drawing in the default colour, bold header,
/// a separator between every body row, cells wrapped when the grid is wider than `avail`.
fn table(header: [&str; 2], body: Vec<[Vec<Span<'static>>; 2]>, avail: usize) -> Lines {
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let head: [Vec<Span<'static>>; 2] = [vec![span(header[0], bold)], vec![span(header[1], bold)]];
    let all: Vec<&[Vec<Span<'static>>; 2]> = std::iter::once(&head).chain(body.iter()).collect();
    let mut w = [0usize; 2];
    for r in &all {
        for c in 0..2 {
            w[c] = w[c].max(spans_width(&r[c]));
        }
    }
    let overhead = 7; // "│ " + " │ " + " │"
    if w[0] + w[1] + overhead > avail {
        let room = avail.saturating_sub(overhead).max(8);
        let first = w[0].min(room / 2).max(4);
        w[0] = first;
        w[1] = room.saturating_sub(first).max(4);
    }
    let line = |l: char, m: char, r: char| {
        Line::from(span(
            format!("{l}{}{m}{}{r}", "─".repeat(w[0] + 2), "─".repeat(w[1] + 2)),
            Style::default(),
        ))
    };
    let mut out: Lines = vec![line('┌', '┬', '┐')];
    for (i, r) in all.iter().enumerate() {
        let wrapped: Vec<Vec<Vec<Span<'static>>>> = (0..2)
            .map(|c| wrap_spans(&r[c], w[c], WrapMode::Word))
            .collect();
        let h = wrapped[0].len().max(wrapped[1].len());
        for k in 0..h {
            let mut spans: Vec<Span<'static>> = vec![Span::raw("│ ")];
            for c in 0..2 {
                let cell = wrapped[c].get(k).cloned().unwrap_or_default();
                let cw = spans_width(&cell);
                spans.extend(cell);
                spans.push(Span::raw(" ".repeat(w[c].saturating_sub(cw))));
                spans.push(Span::raw(if c == 0 { " │ " } else { " │" }));
            }
            out.push(Line::from(spans));
        }
        out.push(if i + 1 == all.len() {
            line('└', '┴', '┘')
        } else {
            line('├', '┼', '┤')
        });
    }
    out
}

/// `/hotkeys`: the tables of spec 11.11 without the rows for features piw does not have (jump to
/// character, yank cycling, images).
pub fn hotkeys(cx: &Cx, km: &Keymap) -> Lines {
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let avail = cx.width.saturating_sub(2).max(10) as usize;
    let mut body: Lines = vec![blank()];
    for (name, list) in rows(km) {
        body.push(indent(Line::from(span(name, bold)), 1));
        body.push(blank());
        let b: Vec<[Vec<Span<'static>>; 2]> = list
            .iter()
            .map(|(k, d)| [cell_spans(k, cx), vec![Span::raw((*d).to_string())]])
            .collect();
        for l in table(["Key", "Action"], b, avail) {
            body.push(indent(l, 1));
        }
        body.push(blank());
    }
    title_block(cx, "Keyboard Shortcuts", body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::PiTheme;
    use std::time::Duration;
    use tuikit::width::display_width;

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

    fn t(l: &Line) -> String {
        l.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn hotkeys_grid_matches_the_capture_shape() {
        let rows: Vec<String> = hotkeys(&cx(120), &Keymap::new()).iter().map(t).collect();
        assert_eq!(rows[1], "─".repeat(120));
        assert_eq!(rows[2], " Keyboard Shortcuts");
        assert!(rows.contains(&" Navigation".to_string()));
        assert!(
            rows.iter()
                .any(|r| r.starts_with(" │ Up / Down / Left/Ctrl+B / Right/Ctrl+F")),
            "{rows:#?}"
        );
        assert!(rows
            .iter()
            .any(|r| r.contains("│ Shift+Tab") && r.contains("Cycle thinking level")));
        // within one table every row has the same width
        for table in rows.split(|r| r.is_empty()) {
            let w: Vec<usize> = table
                .iter()
                .filter(|r| r.starts_with(" │") || r.starts_with(" ┌") || r.starts_with(" ├"))
                .map(|r| display_width(r))
                .collect();
            assert!(w.windows(2).all(|p| p[0] == p[1]), "{table:#?}");
        }
    }

    #[test]
    fn hotkeys_fit_an_80_column_screen() {
        let rows = hotkeys(&cx(80), &Keymap::new());
        assert!(rows.iter().all(|l| display_width(&t(l)) <= 80));
    }

    #[test]
    fn session_block_lists_counts_and_hides_empty_sections() {
        let s = SessionStats {
            id: "abc",
            path: "~/.wizard/sessions/abc".into(),
            users: 1,
            assistants: 1,
            ..Default::default()
        };
        let rows: Vec<String> = session_info(&cx(100), &s).iter().map(t).collect();
        assert_eq!(rows[0], "Session Info");
        assert!(rows.contains(&"ID: abc".to_string()));
        assert!(rows.contains(&"Tools: 0 calls, 0 results".to_string()));
        assert!(!rows.contains(&"Tokens".to_string()));
    }
}
