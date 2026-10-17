//! `/tutorial`: nine topics, each a short page (spec 6.13). The pages are the ones Grok Build
//! ships (Apache 2.0, SpaceXAI); a topic that describes something grokw does not have starts with
//! a one-line note saying so.

use std::collections::HashSet;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use tuikit::paint::put_str;
use tuikit::width::{display_width, truncate};

use super::widgets::{self, Hint};
use super::{modal_frame, modal_rect, page, Modal};
use crate::app::App;
use crate::ui::{st, Layout};

/// Title, blurb, page and a note for what grokw does not do.
pub const TOPICS: &[(&str, &str, &str, &str)] = &[
    ("Coming from Claude, Cursor, or Codex?", "your settings, rules & skills carry over", include_str!("../../../docs/tutorial/01-coming-from-another-tool.md"), "grokw note: wizard reads its own config; importing another tool's settings is not built."),
    ("Your First Prompt", "send, queue, cancel", include_str!("../../../docs/tutorial/02-first-prompt.md"), ""),
    ("Attach Files, Images & Paste", "@files, line ranges, screenshots", include_str!("../../../docs/tutorial/03-attach-and-paste.md"), "grokw note: images are sent to wizard as an @path, the line viewer for @file:range is not built."),
    ("Finding Your Way Around", "focus, scrollback, panes", include_str!("../../../docs/tutorial/04-navigation.md"), ""),
    ("Slash Commands", "/help  /model  /resume  and Ctrl+P", include_str!("../../../docs/tutorial/05-slash-commands.md"), ""),
    ("Parallel Work: Worktrees", "isolated sessions on one repo", include_str!("../../../docs/tutorial/06-worktrees.md"), "grokw note: worktrees are not built in grokw."),
    ("Plan Mode & Permissions", "review the approach before it acts", include_str!("../../../docs/tutorial/07-plan-and-permissions.md"), "grokw note: wizard owns permissions; plan mode works, always-approve and auto do not exist."),
    ("Make It Yours", "just ask: AGENTS.md, memory, themes", include_str!("../../../docs/tutorial/08-make-it-yours.md"), ""),
    ("Where to Go Next", "guides, feedback, and good habits", include_str!("../../../docs/tutorial/09-where-next.md"), ""),
];

#[derive(Clone, Debug, Default)]
pub struct Tutorial {
    pub sel: usize,
    pub top: usize,
    pub viewed: HashSet<usize>,
    /// An open topic and its scroll.
    pub page: Option<(usize, usize)>,
}

pub fn open(app: &mut App) {
    app.enter_session();
    app.modal = Some(Modal::Tutorial(Tutorial::default()));
    app.dirty = true;
}

fn rect(lay: &Layout) -> Rect {
    modal_rect(lay.w, lay.h, 0.6, 100, 44, 4)
}

fn page_rect(lay: &Layout) -> Rect {
    modal_rect(lay.w, lay.h, 0.8, 120, 44, 4)
}

/// The page text: the markdown without its first heading (the title bar shows it), under the note.
fn body(i: usize) -> String {
    let (_, _, md, note) = TOPICS[i];
    let md = match md.split_once('\n') {
        Some((first, rest)) if first.starts_with("# ") => rest.trim_start_matches('\n'),
        _ => md,
    };
    if note.is_empty() {
        md.to_string()
    } else {
        format!("*{note}*\n\n{md}")
    }
}

fn list_hints(t: &Tutorial) -> Vec<(String, String)> {
    // the progress count is the first piece of the footer
    vec![
        (
            format!("{}/{}", t.viewed.len(), TOPICS.len()),
            "explored".into(),
        ),
        ("↑/↓".into(), "navigate".into()),
        ("Enter".into(), "open".into()),
        ("Esc".into(), "done".into()),
    ]
}

fn page_hints(i: usize) -> Vec<(String, String)> {
    let next = match TOPICS.get(i + 1) {
        Some(n) => format!("next: {}", n.0),
        None => "done".to_string(),
    };
    vec![
        ("↑/↓".into(), "scroll".into()),
        ("→".into(), next),
        ("Esc".into(), "list".into()),
    ]
}

fn refs(h: &[(String, String)]) -> Vec<Hint<'_>> {
    h.iter().map(|(k, l)| (k.as_str(), l.as_str())).collect()
}

pub fn draw(buf: &mut Buffer, app: &App, lay: &Layout, t: &Tutorial) {
    let th = &app.theme;
    if let Some((i, scroll)) = t.page {
        let r = page_rect(lay);
        let _ = modal_frame(buf, app, r, TOPICS[i].0);
        page::draw(buf, app, r, &refs(&page_hints(i)), &body(i), scroll);
        return;
    }
    let r = rect(lay);
    let inner = modal_frame(buf, app, r, "Welcome to Wizard");
    if inner.height < 8 {
        return;
    }
    let hs = list_hints(t);
    let hints = refs(&hs);
    widgets::draw_hints(buf, app, r, &hints);
    let x0 = r.x + 3;
    let intro = Rect::new(x0, r.y + 2, r.width.saturating_sub(6), 2);
    put_str(
        buf,
        x0,
        r.y + 2,
        "Quick tips to get the most out of Wizard.",
        st(th.gray_bright),
        intro,
    );
    put_str(
        buf,
        x0,
        r.y + 3,
        "Pick a topic. Esc when you're done.",
        st(th.gray_bright),
        intro,
    );
    let list_y = r.y + 5;
    let f = widgets::foot_rows(r, &hints).max(2);
    let list_end = (r.bottom() - 1).saturating_sub(f);
    let h = list_end.saturating_sub(list_y) as usize;
    let x1 = r.right() - 4;
    let content_w = r.width.saturating_sub(6);
    let narrow = content_w < 64;
    // rows, each a title line and in the narrow form its blurb lines
    struct Row {
        topic: usize,
        blurb: Option<Vec<String>>,
    }
    let mut rows: Vec<Row> = Vec::new();
    for (i, tp) in TOPICS.iter().enumerate() {
        let blurb =
            narrow.then(|| super::shortcuts::wrap(tp.1, (content_w as usize).saturating_sub(5)));
        rows.push(Row { topic: i, blurb });
    }
    // lines from the scroll offset; the selection stays in view
    let mut lines: Vec<(usize, usize)> = Vec::new(); // (topic, 0 title | k+1 blurb line)
    for r_ in &rows {
        lines.push((r_.topic, 0));
        if let Some(b) = &r_.blurb {
            for k in 0..b.len() {
                lines.push((r_.topic, k + 1));
            }
        }
    }
    let first_line_of = |topic: usize| {
        lines
            .iter()
            .position(|l| l.0 == topic && l.1 == 0)
            .unwrap_or(0)
    };
    let last_line_of = |topic: usize| lines.iter().rposition(|l| l.0 == topic).unwrap_or(0);
    let mut top = t.top;
    if first_line_of(t.sel) < top {
        top = first_line_of(t.sel);
    } else if last_line_of(t.sel) >= top + h {
        top = last_line_of(t.sel) + 1 - h;
    }
    let bar = lines.len() > h;
    let x1 = if bar { x1 - 1 } else { x1 };
    for (k, &(ti, part)) in lines.iter().skip(top).take(h).enumerate() {
        let y = list_y + k as u16;
        let selected = ti == t.sel;
        let bg = if selected { th.bg_visual } else { th.bg_base };
        // the band is on the title row only, one cell past the right edge of the text
        let bg = if selected && part == 0 {
            bg
        } else {
            th.bg_base
        };
        if selected && part == 0 {
            widgets::band(buf, app, x0, x1 + 1, y);
        }
        let tp = &TOPICS[ti];
        let clip = Rect::new(x0, y, (x1 + 1).saturating_sub(x0), 1);
        if part == 0 {
            let viewed = t.viewed.contains(&ti);
            let glyph = if viewed { "✓ " } else { "◆ " };
            let gc = if viewed {
                th.accent_success
            } else {
                th.gray_dim
            };
            put_str(buf, x0, y, glyph, Style::new().fg(gc).bg(bg), clip);
            let mut ls = Style::new().fg(th.text_primary).bg(bg);
            if selected {
                ls = ls.add_modifier(Modifier::BOLD);
            }
            if narrow {
                put_str(
                    buf,
                    x0 + 2,
                    y,
                    &truncate(tp.0, (x1 as usize).saturating_sub(x0 as usize + 2)),
                    ls,
                    clip,
                );
            } else {
                // the blurb never takes more than half the room (less its pad cell), the title
                // gets what is left
                let avail = (x1 as usize).saturating_sub(x0 as usize + 2);
                let blurb = truncate(tp.1, (avail / 2).saturating_sub(1));
                let title = truncate(tp.0, avail.saturating_sub(2 + display_width(&blurb)));
                put_str(buf, x0 + 2, y, &title, ls, clip);
                let bx = (x1 as usize).saturating_sub(display_width(&blurb)) as u16;
                put_str(buf, bx, y, &blurb, Style::new().fg(th.gray).bg(bg), clip);
            }
        } else if let Some(b) = &rows[ti].blurb {
            put_str(
                buf,
                x0 + 4,
                y,
                b.get(part - 1).map_or("", String::as_str),
                Style::new().fg(th.gray).bg(bg),
                clip,
            );
        }
    }
    if bar {
        widgets::draw_scrollbar(buf, app, r, list_y, h as u16, lines.len(), top);
    }
}

pub fn key(app: &mut App, key: KeyEvent) {
    let lay = Layout::compute(app);
    let Some(Modal::Tutorial(t)) = app.modal.clone() else {
        return;
    };
    let mut t = t;
    if let Some((i, mut scroll)) = t.page {
        let r = page_rect(&lay);
        let hs = page_hints(i);
        let hints = refs(&hs);
        let b = body(i);
        match key.code {
            KeyCode::Esc => t.page = None,
            KeyCode::Right => {
                if i + 1 < TOPICS.len() {
                    t.viewed.insert(i + 1);
                    t.page = Some((i + 1, 0));
                } else {
                    t.page = None;
                }
            }
            KeyCode::Left => {
                if i > 0 {
                    t.viewed.insert(i - 1);
                    t.page = Some((i - 1, 0));
                }
            }
            _ => {
                page::scroll_key(app, r, &hints, &b, &mut scroll, key);
                t.page = Some((i, scroll));
            }
        }
        app.modal = Some(Modal::Tutorial(t));
        return;
    }
    match key.code {
        KeyCode::Esc => {
            app.modal = None;
            return;
        }
        KeyCode::Down | KeyCode::Char('j') => t.sel = (t.sel + 1).min(TOPICS.len() - 1),
        KeyCode::Up | KeyCode::Char('k') => t.sel = t.sel.saturating_sub(1),
        KeyCode::Home => t.sel = 0,
        KeyCode::End => t.sel = TOPICS.len() - 1,
        KeyCode::Enter => {
            t.viewed.insert(t.sel);
            t.page = Some((t.sel, 0));
        }
        _ => {}
    }
    app.modal = Some(Modal::Tutorial(t));
}
