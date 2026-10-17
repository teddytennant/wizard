// OWNER: input
//! The `ctrl+t` popover: model, effort and mode, with every value the backend offers listed
//! under the focused row. Left and right change the value at once.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use tuikit::paint::{fill, put_str};
use tuikit::width::truncate;

use crate::ui::dialogs::{footer, panel, title, OvCtx};

/// Most option rows shown under the rows; a longer list scrolls around the current value.
const MAX_OPTIONS: usize = 7;

pub fn draw(buf: &mut Buffer, screen: Rect, row: usize, cx: &OvCtx) {
    let p = cx.p;
    let opt_rows = cx
        .settings
        .options
        .iter()
        .map(Vec::len)
        .max()
        .unwrap_or(0)
        .clamp(1, MAX_OPTIONS) as u16;
    // title, gap, three rows, gap, options, gap, footer, pad (the top pad is the frame's)
    let r = panel(buf, screen, 9 + opt_rows, cx);
    let bg = p.raised;
    title(buf, r, "Turn settings", cx);
    for (i, (label, value, available)) in cx.settings.rows.iter().enumerate() {
        let y = r.y + 3 + i as u16;
        let sel = i == row;
        let st = Style::new().bg(if sel { p.line } else { bg });
        fill(buf, Rect::new(r.x + 1, y, r.width.saturating_sub(2), 1), st);
        put_str(
            buf,
            r.x + 2,
            y,
            if sel { cx.g.select } else { " " },
            st.fg(p.accent),
            r,
        );
        put_str(buf, r.x + 4, y, &format!("{label:<8}"), st.fg(p.dim), r);
        let (l, rr) = if *available {
            ("‹ ", " ›")
        } else {
            ("  ", "  ")
        };
        let v = truncate(value, (r.width as usize).saturating_sub(20));
        // A value that cannot be changed is quiet, but not under 4.5:1 on the selection step.
        let col = match (*available, sel) {
            (true, _) => p.text,
            (false, true) => p.dim,
            (false, false) => p.faint,
        };
        // The arrows sit on the selection step, where `faint` is 4.06:1; `dim` clears it.
        let arrow = if sel { p.dim } else { p.faint };
        let mut x = put_str(buf, r.x + 13, y, l, st.fg(arrow), r);
        x = put_str(
            buf,
            x,
            y,
            &v,
            st.fg(col).add_modifier(if sel {
                Modifier::BOLD
            } else {
                Modifier::empty()
            }),
            r,
        );
        put_str(buf, x, y, rr, st.fg(arrow), r);
    }
    // The focused row's choices.
    let oy = r.y + 7;
    if let Some(opts) = cx.settings.options.get(row) {
        let cur = opts.iter().position(|o| o.1).unwrap_or(0);
        let n = MAX_OPTIONS.min(opts.len());
        let first = cur.saturating_sub(n / 2).min(opts.len().saturating_sub(n));
        if opts.is_empty() {
            put_str(
                buf,
                r.x + 13,
                oy,
                "this backend has no choices here",
                Style::new().fg(p.faint).bg(bg),
                r,
            );
        }
        for (k, (name, is_cur)) in opts.iter().enumerate().skip(first).take(n) {
            let y = oy + (k - first) as u16;
            let (mark, st) = if *is_cur {
                (
                    "●",
                    Style::new()
                        .fg(p.accent)
                        .bg(bg)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                (" ", Style::new().fg(p.dim).bg(bg))
            };
            put_str(buf, r.x + 13, y, mark, Style::new().fg(p.accent).bg(bg), r);
            put_str(
                buf,
                r.x + 15,
                y,
                &truncate(name, r.width.saturating_sub(19) as usize),
                st,
                r,
            );
        }
        if first > 0 {
            put_str(
                buf,
                r.right() - 4,
                oy,
                "▴",
                Style::new().fg(p.faint).bg(bg),
                r,
            );
        }
        if first + n < opts.len() {
            put_str(
                buf,
                r.right().saturating_sub(4),
                (oy + n as u16).saturating_sub(1),
                "▾",
                Style::new().fg(p.faint).bg(bg),
                r,
            );
        }
    }
    footer(
        buf,
        r,
        r.bottom().saturating_sub(2),
        &[
            ("left right", "change"),
            ("up down", "move"),
            ("enter", "close"),
        ],
        cx,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::palette::{Depth, Kind, Palette, UNICODE};
    use crate::ui::dialogs::{Overlay, SettingsView};
    use tuikit::testing::TestTerminal;

    fn opts(names: &[&str], cur: usize) -> Vec<(String, bool)> {
        names
            .iter()
            .enumerate()
            .map(|(i, n)| ((*n).to_string(), i == cur))
            .collect()
    }

    fn screen(row: usize) -> String {
        let p = Palette::new(Kind::Hearth, Depth::True);
        let th = p.theme();
        let sv = SettingsView {
            rows: vec![
                ("model".into(), "opus-5.5".into(), true),
                ("effort".into(), "high".into(), true),
                ("mode".into(), "ask".into(), true),
            ],
            options: vec![
                opts(&["sonnet-5.5", "opus-5.5", "haiku-4.5"], 1),
                opts(&["low", "medium", "high", "xhigh", "max"], 2),
                opts(&["ask", "edits", "plan"], 0),
            ],
        };
        let cx = OvCtx {
            p: &p,
            theme: &th,
            g: &UNICODE,
            settings: &sv,
        };
        let mut ov = Overlay::Settings { row };
        let mut t = TestTerminal::new(100, 30);
        t.draw(|b, a| {
            ov.draw(b, a, &cx);
        });
        t.plain()
    }

    #[test]
    fn the_focused_row_lists_what_the_backend_offers_with_the_current_one_marked() {
        let s = screen(0);
        assert!(
            s.contains("● opus-5.5") && s.contains("sonnet-5.5") && s.contains("haiku-4.5"),
            "{s}"
        );
        let s = screen(1);
        assert!(
            s.contains("● high") && s.contains("xhigh") && s.contains("max"),
            "{s}"
        );
        assert!(
            !s.contains("sonnet-5.5\n"),
            "only the focused row's list shows"
        );
    }
}
