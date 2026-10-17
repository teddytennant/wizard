// OWNER: input
//! The key help overlay. Every row comes from [`BINDINGS`], so a key that is bound is listed
//! and a key that is not bound is not. Two columns when there is room (design section d).

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use tuikit::paint::put_str;
use tuikit::width::{display_width, truncate};

use crate::keys::{Scope, BINDINGS};
use crate::ui::dialogs::{dlg_width, footer, tall_panel, title, OvCtx, INSET};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Line {
    Blank,
    Head(&'static str),
    /// The first key on the first row, with the description; `None` on a continuation row of
    /// a wrapped description. A binding with alternates lists only its first key here.
    Row(Option<String>, String),
    /// The other keys of a binding, `also shift+enter, alt+enter`, under its description.
    Also(String),
}

/// Word-wrap `s` to `w` columns (at least one word per row).
fn wrap(s: &str, w: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    for word in s.split_whitespace() {
        if !cur.is_empty() && display_width(&cur) + 1 + display_width(word) > w {
            out.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(word);
    }
    if !cur.is_empty() || out.is_empty() {
        out.push(cur);
    }
    out
}

/// A binding's `keys` is a list of alternatives; the first is the one the row leads with.
fn split_keys(keys: &str) -> (&str, String) {
    let mut it = keys.split_whitespace();
    let first = it.next().unwrap_or("");
    (first, it.collect::<Vec<_>>().join(", "))
}

/// Lines of one scope: heading, then one row per binding. `key_w` is 0 when the panel is too
/// narrow for two columns, and the description goes under its key instead.
fn section(scope: Scope, key_w: usize, desc_w: usize) -> Vec<Line> {
    let mut v = vec![Line::Head(scope.title())];
    for bd in BINDINGS.iter().filter(|b| b.scope == scope) {
        let (first, also) = split_keys(bd.keys);
        let lines = wrap(bd.help, desc_w.max(8));
        if key_w == 0 {
            v.push(Line::Row(Some(first.to_string()), String::new()));
            v.extend(lines.into_iter().map(|d| Line::Row(None, d)));
        } else {
            for (i, d) in lines.into_iter().enumerate() {
                v.push(Line::Row((i == 0).then(|| first.to_string()), d));
            }
        }
        if !also.is_empty() {
            v.push(Line::Also(also));
        }
    }
    v
}

/// The widest first key over every binding, capped.
pub fn key_width(cap: usize) -> usize {
    BINDINGS
        .iter()
        .map(|b| display_width(split_keys(b.keys).0))
        .max()
        .unwrap_or(8)
        .min(cap)
}

/// A description narrower than this is not worth a column: it goes under its key.
const MIN_DESC: usize = 24;

/// Every line of the page for a panel `inner` columns wide (the dialog is at most 80, so
/// there is one column; two never fit once the key column is no wider than its longest key).
pub fn columns(inner: usize) -> (Vec<Line>, usize) {
    let mut key_w = key_width(14);
    // Text starts 2 in; the gap between key and description is 2.
    let mut desc_w = inner.saturating_sub(2 + key_w + 2);
    if desc_w < MIN_DESC {
        key_w = 0;
        desc_w = inner.saturating_sub(4);
    }
    let mut all = Vec::new();
    for (i, scope) in Scope::ALL.iter().enumerate() {
        if i > 0 {
            all.push(Line::Blank);
        }
        all.extend(section(*scope, key_w, desc_w));
    }
    (all, key_w)
}

pub fn draw(buf: &mut Buffer, screen: Rect, scroll: &mut usize, cx: &OvCtx) {
    let p = cx.p;
    let w = dlg_width(screen);
    let inner = (w.saturating_sub(2 * INSET)) as usize;
    let (lines, key_w) = columns(inner);
    // top pad (the frame's), title, gap, rows, gap, footer, bottom pad
    let cap = screen.height.saturating_sub(2 + 1 + 5) as usize;
    let rows = lines.len().min(cap.max(1));
    let r = tall_panel(buf, screen, rows as u16 + 5, cx);
    title(buf, r, "Keys", cx);
    let bg = p.raised;
    let rows = (r.height.saturating_sub(6) as usize).min(rows);
    *scroll = (*scroll).min(lines.len().saturating_sub(rows));
    let x0 = r.x + INSET;
    let width = inner;
    let dim = Style::new().fg(p.dim).bg(bg);
    for (i, line) in lines.iter().skip(*scroll).take(rows).enumerate() {
        let y = r.y + 3 + i as u16;
        let key_x = x0 + 2;
        let desc_x = if key_w == 0 {
            x0 + 4
        } else {
            key_x + key_w as u16 + 2
        };
        match line {
            Line::Blank => {}
            Line::Head(t) => {
                put_str(
                    buf,
                    x0,
                    y,
                    t,
                    Style::new()
                        .fg(p.accent)
                        .bg(bg)
                        .add_modifier(Modifier::BOLD),
                    r,
                );
            }
            Line::Row(k, d) => {
                if let Some(k) = k {
                    put_str(
                        buf,
                        key_x,
                        y,
                        &truncate(k, width.saturating_sub(2)),
                        Style::new().fg(p.text).bg(bg).add_modifier(Modifier::BOLD),
                        r,
                    );
                }
                put_str(
                    buf,
                    desc_x,
                    y,
                    &truncate(d, width.saturating_sub((desc_x - x0) as usize)),
                    dim,
                    r,
                );
            }
            Line::Also(a) => {
                put_str(
                    buf,
                    desc_x,
                    y,
                    &truncate(
                        &format!("also {a}"),
                        width.saturating_sub((desc_x - x0) as usize),
                    ),
                    Style::new().fg(p.faint).bg(bg),
                    r,
                );
            }
        }
    }
    let mut hints: Vec<(&str, &str)> = Vec::new();
    if lines.len() > rows {
        hints.push(("up down", "scroll"));
    }
    hints.push(("esc", "close"));
    footer(buf, r, r.bottom().saturating_sub(2), &hints, cx);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::palette::{Depth, Kind, Palette, UNICODE};
    use crate::ui::dialogs::{Overlay, SettingsView};
    use tuikit::testing::TestTerminal;

    fn draw_at(w: u16, h: u16) -> String {
        let p = Palette::new(Kind::Hearth, Depth::True);
        let th = p.theme();
        let sv = SettingsView::default();
        let cx = OvCtx {
            p: &p,
            theme: &th,
            g: &UNICODE,
            settings: &sv,
        };
        let mut ov = Overlay::Help { scroll: 0 };
        let mut t = TestTerminal::new(w, h);
        t.draw(|b, a| {
            ov.draw(b, a, &cx);
        });
        t.plain()
    }

    /// The page for a screen: every row of every width, scrolled to the top, with room for it all.
    fn page(w: u16) -> String {
        draw_at(w, 240)
    }

    #[test]
    fn every_bound_key_is_listed_with_its_help_at_every_width() {
        for inner in [28, 36, 52, 72] {
            let (lines, _) = columns(inner);
            // (first key, description with wrapped rows joined, alternates)
            let mut rows: Vec<(String, String, String)> = Vec::new();
            for l in &lines {
                match l {
                    Line::Row(Some(k), d) => rows.push((k.clone(), d.clone(), String::new())),
                    Line::Row(None, d) => {
                        if let Some(last) = rows.last_mut() {
                            if !last.1.is_empty() {
                                last.1.push(' ');
                            }
                            last.1.push_str(d);
                        }
                    }
                    Line::Also(a) => {
                        if let Some(last) = rows.last_mut() {
                            last.2 = a.clone();
                        }
                    }
                    _ => {}
                }
            }
            for bd in BINDINGS {
                let (first, also) = split_keys(bd.keys);
                let want = bd.help.split_whitespace().collect::<Vec<_>>().join(" ");
                assert!(
                    rows.iter()
                        .any(|(k, d, a)| k == first && *d == want && *a == also),
                    "inner {inner}: `{}` / `{}` is not in the help",
                    bd.keys,
                    bd.help
                );
            }
        }
    }

    #[test]
    fn every_description_is_on_screen_at_44_60_80_and_120_columns() {
        // Finding 3: at 44 columns the keys had no descriptions at all, and at 120 the
        // dialog was 112 wide with a 24 column hole between key and description.
        for w in [44u16, 60, 80, 120, 160] {
            let s = page(w);
            let widest = s.lines().map(display_width).max().unwrap_or(0);
            let dlg = (w - 8).min(80) as usize;
            assert!(widest <= (w as usize), "{w}: overflow");
            for bd in BINDINGS {
                for word in bd.help.split_whitespace() {
                    assert!(
                        s.contains(word),
                        "{w}: `{word}` of `{}` is missing\n{s}",
                        bd.help
                    );
                }
                let (first, _) = split_keys(bd.keys);
                assert!(s.contains(first), "{w}: key `{first}` missing");
            }
            // The panel is as wide as the rule says, never the 112 or 124 of round 1.
            let panel_rows = s.lines().filter(|l| l.contains("Everywhere")).count();
            assert_eq!(panel_rows, 1);
            let l = s.lines().find(|l| l.contains("Everywhere")).unwrap();
            let start = l.find("Everywhere").unwrap();
            assert!(display_width(&l[..start]) >= (w as usize - dlg) / 2, "{w}");
        }
    }

    #[test]
    fn the_key_column_fits_its_longest_key_and_is_not_a_fixed_thirty() {
        let kw = key_width(14);
        assert!((8..=14).contains(&kw), "{kw}");
        // The second row of the page carries a key and its description close together.
        let s = page(120);
        let row = s.lines().find(|l| l.contains("ctrl+c")).unwrap();
        let k = row.find("ctrl+c").unwrap();
        let d = row.find("interrupt").unwrap();
        assert!(
            d - k <= kw + 3,
            "gap of {} between key and description: {row:?}",
            d - k
        );
        // Alternates move to their own faint line.
        assert!(s.contains("also shift+enter, alt+enter"), "{s}");
    }

    #[test]
    fn the_overlay_draws_every_key_on_a_tall_screen() {
        let s = draw_at(120, 120);
        assert!(s.contains("Nav mode") && s.contains("Resume dialog") && s.contains("Editing"));
        assert!(s.contains("esc close"), "{s}");
    }
}
