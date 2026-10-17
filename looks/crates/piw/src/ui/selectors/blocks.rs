//! The `/help` block: the slash commands, then Pi's keyboard shortcut tables (spec 11.11). It
//! draws its own rules and padding.

use agent_core::SlashCommand;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use tuikit::width::{spans_width, wrap_spans, WrapMode};

use crate::keys::{Action, Keymap};
use crate::theme::Tok;
use crate::ui::{blank, indent, rule, span, Cx, Lines};

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

/// `/help`: the commands in `menu`, then the key tables of spec 11.11 without the rows for
/// features piw does not have (jump to character, yank cycling, images).
pub fn help(cx: &Cx, km: &Keymap, menu: &[SlashCommand]) -> Lines {
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let avail = cx.width.saturating_sub(2).max(10) as usize;
    let mut body: Lines = vec![blank()];
    let mut section = |name: &'static str, header, b| {
        body.push(indent(Line::from(span(name, bold)), 1));
        body.push(blank());
        for l in table(header, b, avail) {
            body.push(indent(l, 1));
        }
        body.push(blank());
    };
    let commands = menu
        .iter()
        .map(|c| {
            let usage = format!("/{} {}", c.name, c.input_hint);
            [
                cell_spans(&Cell::Text(usage.trim_end().into()), cx),
                vec![Span::raw(c.description.clone())],
            ]
        })
        .collect();
    section("Commands", ["Command", "Description"], commands);
    for (name, list) in rows(km) {
        let b = list
            .iter()
            .map(|(k, d)| [cell_spans(k, cx), vec![Span::raw((*d).to_string())]])
            .collect();
        section(name, ["Key", "Action"], b);
    }
    title_block(cx, "Help", body)
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

    fn menu() -> Vec<SlashCommand> {
        crate::commands::items(&agent_core::mock::commands())
    }

    #[test]
    fn help_grid_matches_the_capture_shape() {
        let rows: Vec<String> = help(&cx(120), &Keymap::new(), &menu())
            .iter()
            .map(t)
            .collect();
        assert_eq!(rows[1], "─".repeat(120));
        assert_eq!(rows[2], " Help");
        assert_eq!(rows[5], " Commands");
        assert!(rows.contains(&" Navigation".to_string()));
        assert!(
            rows.iter().any(
                |r| r.starts_with(" │ /model <provider>/<model>") && r.contains("switch model")
            ),
            "{rows:#?}"
        );
        assert!(rows.iter().any(|r| r.starts_with(" │ /copy ")), "{rows:#?}");
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
    fn help_fits_an_80_column_screen() {
        let rows = help(&cx(80), &Keymap::new(), &menu());
        assert!(rows.iter().all(|l| display_width(&t(l)) <= 80));
    }
}
