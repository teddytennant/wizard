// OWNER: header (logo, hints, startup notices)
//! The startup header: logo and version, key hints, the docs tagline. `ctrl+o` expands it into one
//! line per instruction. There is no loaded-resources listing (wizard reports no context, skills or
//! prompts over ACP), so the compact text is `quietStartup: "header"`'s. Spec 4.1, 4.2, 14.3.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use tuikit::width::{wrap_spans, WrapMode};

use super::{blank, indent, span, Cx, Lines};
use crate::keys::{Action, Keymap};
use crate::theme::{Tok, LOGO_CORAL};

pub const TAGLINE: &str =
    "Wizard can explain its own features and look up its docs. Ask it how to use or extend Wizard.";

fn logo1(version: &str, cx: &Cx) -> Vec<Span<'static>> {
    let coral = Style::default().fg(LOGO_CORAL).add_modifier(Modifier::BOLD);
    vec![
        span("wizard", coral),
        span("  ", Style::default()),
        span(format!("v{version}"), cx.th().fg(Tok::Dim)),
    ]
}

/// What leads the hint line under the name: nothing, the name sits on the row above.
fn logo2() -> Vec<Span<'static>> {
    Vec::new()
}

/// `key` in `dim`, ` description` in `muted`.
fn hint(cx: &Cx, key: &str, desc: &str) -> Vec<Span<'static>> {
    vec![
        span(key.to_string(), cx.th().fg(Tok::Dim)),
        span(format!(" {desc}"), cx.th().fg(Tok::Muted)),
    ]
}

fn compact_hint_line(cx: &Cx, km: &Keymap) -> Vec<Span<'static>> {
    let sep = || span(" · ", cx.th().fg(Tok::Muted));
    let mut v = hint(cx, &km.key(Action::Interrupt), "interrupt");
    v.push(sep());
    v.extend(hint(
        cx,
        &format!("{}/{}", km.key(Action::Clear), km.key(Action::Exit)),
        "clear/exit",
    ));
    v.push(sep());
    v.extend(hint(cx, "/", "commands"));
    v.push(sep());
    v.extend(hint(cx, "!", "bash"));
    v.push(sep());
    v.extend(hint(cx, &km.key(Action::ToolsExpand), "more"));
    v
}

fn instructions(cx: &Cx, km: &Keymap) -> Vec<Vec<Span<'static>>> {
    let k = |a| km.key(a);
    let list: Vec<(String, &str)> = vec![
        (k(Action::Interrupt), "to interrupt"),
        (k(Action::Clear), "to clear"),
        (format!("{} twice", k(Action::Clear)), "to exit"),
        (k(Action::Exit), "to exit (empty)"),
        (k(Action::Suspend), "to suspend"),
        ("ctrl+k".into(), "to delete to end"),
        (k(Action::ThinkingCycle), "to cycle thinking level"),
        ("ctrl+p/shift+ctrl+p".into(), "to cycle models"),
        (k(Action::ModelSelect), "to select model"),
        (k(Action::ToolsExpand), "to expand tools"),
        (k(Action::ThinkingToggle), "to expand thinking"),
        (k(Action::ExternalEditor), "for external editor"),
        ("/".into(), "for commands"),
        ("!".into(), "to run bash"),
        ("!!".into(), "to run bash (no context)"),
        (k(Action::FollowUp), "to queue follow-up"),
        (k(Action::Dequeue), "to edit all queued messages"),
    ];
    list.into_iter().map(|(key, d)| hint(cx, &key, d)).collect()
}

/// Header rows: a spacer, the text block (padding 1, wrapped at `width - 2`), a spacer.
pub fn render(cx: &Cx, km: &Keymap) -> Lines {
    let dim = cx.th().fg(Tok::Dim);
    let mut logical: Vec<Vec<Span<'static>>> = Vec::new();
    logical.push(logo1(&cx.version, cx));
    if cx.expanded {
        let mut ins = instructions(cx, km).into_iter();
        let mut first = logo2();
        first.extend(ins.next().unwrap_or_default());
        logical.push(first);
        logical.extend(ins);
        logical.push(Vec::new());
        logical.push(vec![span(TAGLINE, dim)]);
    } else {
        let mut l2 = logo2();
        l2.extend(compact_hint_line(cx, km));
        logical.push(l2);
        logical.push(vec![span(
            format!(
                "Press {} to show full startup help.",
                km.key(Action::ToolsExpand)
            ),
            dim,
        )]);
        logical.push(Vec::new());
        logical.push(vec![span(TAGLINE, dim)]);
    }
    let w = cx.width.saturating_sub(2).max(1) as usize;
    let mut out: Lines = vec![blank()];
    for l in logical {
        if l.is_empty() {
            out.push(blank());
            continue;
        }
        for row in wrap_spans(&l, w, WrapMode::Word) {
            out.push(indent(Line::from(row), 1));
        }
    }
    out.push(blank());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::PiTheme;

    fn cx(width: u16, expanded: bool) -> Cx {
        Cx {
            theme: PiTheme::dark(),
            width,
            expanded,
            hide_thinking: false,
            out_pad: 1,
            cwd: "/home/me".into(),
            home: "/home/me".into(),
            clock: Default::default(),
            version: "1.0.3".into(),
        }
    }

    fn text(l: &Line) -> String {
        l.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn compact_header_rows_at_120() {
        let rows: Vec<String> = render(&cx(120, false), &Keymap::new())
            .iter()
            .map(text)
            .collect();
        assert_eq!(rows[0], "");
        assert_eq!(rows[1], " wizard  v1.0.3");
        assert_eq!(
            rows[2],
            " escape interrupt · ctrl+c/ctrl+d clear/exit · / commands · ! bash · ctrl+o more"
        );
        assert_eq!(rows[3], " Press ctrl+o to show full startup help.");
        assert_eq!(rows[4], "");
        assert_eq!(rows[5], format!(" {TAGLINE}"));
    }

    #[test]
    fn hints_wrap_after_bash_at_72() {
        let rows: Vec<String> = render(&cx(72, false), &Keymap::new())
            .iter()
            .map(text)
            .collect();
        assert!(rows[2].ends_with("! bash ·"), "{:?}", rows[2]);
        assert_eq!(rows[3], " ctrl+o more");
    }
}
