// OWNER: footer (home footer row, home and session hint rows)
//! The status rows: the home screen's directory and version footer, the hint row under the home
//! prompt (`tab agents  ctrl+p commands`) and the session hint row (spinner, `esc interrupt`,
//! cwd, usage).

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use tuikit::paint::{fill, put_str};
use tuikit::spinner::Scanner;
use tuikit::width::{display_width, truncate};

use crate::app::App;
use crate::keys::Action;

/// `~`-abbreviated cwd with `:branch` appended.
pub fn dir_label(app: &App) -> String {
    let mut s = abbreviate_home(&app.cwd);
    if let Some(b) = &app.branch {
        s.push(':');
        s.push_str(b);
    }
    s
}

pub fn abbreviate_home(p: &str) -> String {
    match std::env::var("HOME") {
        Ok(h) if !h.is_empty() && p == h => "~".into(),
        Ok(h) if !h.is_empty() && p.starts_with(&format!("{h}/")) => format!("~{}", &p[h.len()..]),
        _ => p.to_string(),
    }
}

/// Split a path label into rows no wider than `w`. Rows break after a `-` or a space when one
/// fits, like opencode's word wrap; a segment with none is cut at the width.
pub(crate) fn wrap_chars(s: &str, w: usize) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut start = 0;
    while start < chars.len() {
        let mut width = 0;
        let mut end = start;
        let mut last_break = None;
        while end < chars.len() {
            let cw = display_width(&chars[end].to_string());
            if width + cw > w && end > start {
                break;
            }
            width += cw;
            end += 1;
            if matches!(chars[end - 1], '-' | ' ' | '/') {
                last_break = Some(end);
            }
        }
        if end < chars.len() {
            if let Some(b) = last_break {
                end = b;
            }
        }
        out.push(chars[start..end].iter().collect::<String>());
        start = end;
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

fn home_path_rows(app: &App, w: u16) -> Vec<String> {
    let version = app.config.version();
    let avail = (w as usize)
        .saturating_sub(4 + display_width(version) + 4)
        .max(8);
    wrap_chars(&dir_label(app), avail)
}

/// Rows the home footer takes: padding, path rows, padding.
pub fn home_footer_height(app: &App, w: u16) -> u16 {
    2 + home_path_rows(app, w).len() as u16
}

/// Draw the home footer into the bottom `home_footer_height` rows.
pub fn home_footer(buf: &mut Buffer, screen: Rect, app: &App) {
    let rows = home_path_rows(app, screen.width);
    let h = 2 + rows.len() as u16;
    let top = screen.bottom().saturating_sub(h);
    let t = &app.theme;
    let style = Style::new().fg(t.text_muted).bg(t.background);
    for (i, r) in rows.iter().enumerate() {
        put_str(buf, screen.x + 2, top + 1 + i as u16, r, style, screen);
    }
    let vw = display_width(app.config.version()) as u16;
    put_str(
        buf,
        screen.right().saturating_sub(2 + vw),
        top + 1,
        app.config.version(),
        style,
        screen,
    );
}

/// Draw `key label` pairs right-aligned so the last label ends at `right` (inclusive of col).
fn hint_pairs(
    buf: &mut Buffer,
    y: u16,
    right_edge: u16,
    left_limit: u16,
    pairs: &[(String, String)],
    app: &App,
    clip: Rect,
) {
    let t = &app.theme;
    let total: usize = pairs
        .iter()
        .map(|(k, l)| display_width(k) + 1 + display_width(l))
        .sum::<usize>()
        + 2 * pairs.len().saturating_sub(1);
    let mut x = (right_edge as usize)
        .saturating_sub(total)
        .max(left_limit as usize) as u16;
    for (i, (k, l)) in pairs.iter().enumerate() {
        if i > 0 {
            x += 2;
        }
        x = put_str(buf, x, y, k, Style::new().fg(t.text).bg(t.background), clip);
        x = put_str(
            buf,
            x,
            y,
            &format!(" {l}"),
            Style::new().fg(t.text_muted).bg(t.background),
            clip,
        );
    }
}

/// Hint row under the home prompt: right-aligned to the prompt's right edge.
pub fn home_hints(buf: &mut Buffer, prompt: Rect, y: u16, app: &App) {
    let pairs = if app.prompt.shell {
        vec![("esc".to_string(), "exit shell mode".to_string())]
    } else {
        vec![
            (app.keymap.first(&Action::AgentCycle), "agents".to_string()),
            (app.keymap.first(&Action::Palette), "commands".to_string()),
        ]
    };
    let clip = Rect::new(prompt.x, y, prompt.width, 1);
    hint_pairs(buf, y, prompt.right(), prompt.x, &pairs, app, clip);
}

/// Tokens in the context window: what the backend reports, else input plus output.
pub fn context_tokens(u: &agent_core::Usage) -> u64 {
    if u.context_tokens > 0 {
        u.context_tokens
    } else {
        u.input_tokens + u.output_tokens
    }
}

/// `12.3K (6%)` or with a cost, `None` before any usage arrives.
pub fn usage_text(app: &App) -> Option<String> {
    let u = &app.transcript.usage;
    let tokens = context_tokens(u);
    if tokens == 0 {
        return None;
    }
    let mut s = number(tokens);
    if u.context_window > 0 {
        s.push_str(&format!(
            " ({}%)",
            ((tokens as f64 / u.context_window as f64) * 100.0).round() as u64
        ));
    }
    if let Some(c) = u.cost_usd.filter(|c| *c > 0.0) {
        s.push_str(&format!(" · ${c:.2}"));
    }
    Some(s)
}

/// opencode's `Locale.number`.
pub fn number(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1e6)
    } else if n >= 1000 {
        format!("{:.1}K", n as f64 / 1e3)
    } else {
        n.to_string()
    }
}

/// One text in the right group of the hint row. `wraps` is false for the usage text, which
/// opencode sets to `wrapMode none`.
struct RightItem {
    key: Option<String>,
    label: String,
    wraps: bool,
}

impl RightItem {
    fn width(&self) -> usize {
        self.key.as_ref().map_or(0, |k| display_width(k) + 1) + display_width(&self.label)
    }

    /// Lines of `(text, is_key)` runs once squeezed into `w` cells.
    fn lines(&self, w: usize) -> Vec<Vec<(String, bool)>> {
        let Some(k) = &self.key else {
            return vec![vec![(self.label.clone(), false)]];
        };
        if !self.wraps || self.width() <= w {
            return vec![vec![(k.clone(), true), (format!(" {}", self.label), false)]];
        }
        vec![
            vec![(truncate(k, w), true)],
            vec![(truncate(&self.label, w), false)],
        ]
    }
}

/// Where everything on the session hint row goes. Mirrors opencode's flex row: the path box and
/// the right group shrink in proportion to their natural widths when they do not both fit; the
/// path then wraps, and the right group's wrappable texts wrap.
pub struct HintLayout {
    pub height: u16,
    left: Vec<String>,
    right: Vec<RightItem>,
    /// Width each right item is laid out at.
    right_w: Vec<usize>,
    /// Column offset of the right group from the row's left edge.
    right_x: u16,
}

pub fn hint_layout(app: &App, width: u16) -> HintLayout {
    let w = width as usize;
    let mut right: Vec<RightItem> = Vec::new();
    if app.transcript.busy && app.busy_note.is_some() {
        // while a retry is showing, the usage and command hints are hidden
    } else if app.prompt.shell {
        right.push(RightItem {
            key: Some("esc".into()),
            label: "exit shell mode".into(),
            wraps: true,
        });
    } else {
        match usage_text(app) {
            Some(u) => right.push(RightItem {
                key: None,
                label: u,
                wraps: false,
            }),
            None => right.push(RightItem {
                key: Some(app.keymap.first(&Action::AgentCycle)),
                label: "agents".into(),
                wraps: true,
            }),
        }
        right.push(RightItem {
            key: Some(app.keymap.first(&Action::Palette)),
            label: "commands".into(),
            wraps: true,
        });
    }
    let basis: Vec<usize> = right.iter().map(RightItem::width).collect();
    let right_basis: usize = basis.iter().sum::<usize>() + 2 * basis.len().saturating_sub(1);
    let path = if app.transcript.busy {
        String::new()
    } else {
        app.cwd.clone()
    };
    let left_basis = if path.is_empty() {
        0
    } else {
        1 + display_width(&path)
    };

    let free = w as i64 - (left_basis + right_basis) as i64;
    let (left_w, right_w_total) = if free >= 0 || left_basis == 0 {
        (left_basis, right_basis.min(w))
    } else {
        let total = (left_basis + right_basis) as f64;
        let cut_left = ((-free) as f64 * left_basis as f64 / total).round() as usize;
        let l = left_basis - cut_left.min(left_basis);
        (l, w.saturating_sub(l))
    };
    // The right group's own flex row squeezes every item by its share of the deficit (rounding
    // up, as opencode's layout does); the usage text cannot wrap, so it just overflows.
    let deficit = right_basis.saturating_sub(right_w_total);
    let basis_total: usize = basis.iter().sum();
    let right_w: Vec<usize> = basis
        .iter()
        .map(|b| {
            if deficit > 0 && basis_total > 0 {
                let cut = (deficit as f64 * *b as f64 / basis_total as f64).ceil() as usize;
                b.saturating_sub(cut).max(1)
            } else {
                *b
            }
        })
        .collect();
    let left = if path.is_empty() || left_w < 2 {
        Vec::new()
    } else {
        wrap_chars(&path, left_w - 1)
    };
    let right_lines = right
        .iter()
        .zip(&right_w)
        .map(|(r, w)| r.lines(*w).len())
        .max()
        .unwrap_or(1);
    let height = left.len().max(right_lines).max(1) as u16;
    HintLayout {
        height,
        left,
        right,
        right_w,
        right_x: width.saturating_sub(right_w_total as u16),
    }
}

/// Rows the hint row needs at column width `width`.
pub fn hint_height(app: &App, width: u16) -> u16 {
    hint_layout(app, width).height
}

/// Session hint row under the prompt box (`row.height` rows, from [`hint_height`]).
pub fn session_hints(buf: &mut Buffer, row: Rect, app: &App) {
    if row.width < 10 || row.height == 0 {
        return;
    }
    let t = &app.theme;
    let clip = row;
    fill(buf, row, Style::new().bg(t.background));
    let first = Rect { height: 1, ..row };
    let lay = hint_layout(app, row.width);
    let left = row.x + 1;
    if app.transcript.busy {
        let color = app.agent.color(t);
        // opencode blends the trail over the page background, not the prompt panel
        let sc = Scanner::prompt(color, t.background);
        let mut x = left;
        for sp in sc.spans(app.anim_elapsed()) {
            x = put_str(buf, x, row.y, &sp.content, sp.style.bg(t.background), first);
        }
        x += 1;
        let retry = app.busy_note.as_deref().map(retry_text);
        // `interrupting…` and `not responding` replace the hint: the key press registered, or
        // the backend is the problem, and `esc interrupt` would say neither
        let status = app.interrupt_status();
        if let Some(r) = &retry {
            // the message box never shrinks, so on a narrow row it pushes `esc interrupt` out
            x = put_str(
                buf,
                x,
                row.y,
                r,
                Style::new().fg(t.error).bg(t.background),
                first,
            );
        }
        x = if retry.is_some() {
            // space-between: the interrupt hint sits at the right edge
            let w = if let Some((text, _)) = &status {
                text.chars().count()
            } else if app.esc_armed() {
                "esc again to interrupt".len()
            } else {
                "esc interrupt".len()
            };
            (row.right().saturating_sub(w as u16)).max(x + 1)
        } else {
            x + 1
        };
        if let Some((text, warn)) = &status {
            put_str(
                buf,
                x,
                row.y,
                text,
                Style::new()
                    .fg(if *warn { t.warning } else { t.text_muted })
                    .bg(t.background),
                first,
            );
        } else if app.esc_armed() {
            put_str(
                buf,
                x,
                row.y,
                "esc again to interrupt",
                Style::new().fg(t.primary).bg(t.background),
                first,
            );
        } else {
            x = put_str(
                buf,
                x,
                row.y,
                "esc",
                Style::new().fg(t.text).bg(t.background),
                first,
            );
            put_str(
                buf,
                x,
                row.y,
                " interrupt",
                Style::new().fg(t.text_muted).bg(t.background),
                first,
            );
        }
    } else {
        for (i, l) in lay.left.iter().enumerate() {
            put_str(
                buf,
                left,
                row.y + i as u16,
                l,
                Style::new().fg(t.text_muted).bg(t.background),
                clip,
            );
        }
    }
    let mut x = row.x + lay.right_x;
    let n = lay.right.len();
    for (k, (item, w)) in lay.right.iter().zip(&lay.right_w).enumerate() {
        // A text that cannot wrap and does not fit its box runs into the gap before the next
        // item, but opencode stops it one cell short of that item (measured: `$0.` then a
        // blank, with the next item 2 cells past the box).
        let reach = if item.wraps || k + 1 == n || item.width() <= *w + 2 {
            row.width as usize
        } else {
            *w + 1
        };
        for (i, runs) in item.lines(*w).into_iter().enumerate() {
            let y = row.y + i as u16;
            let clip = Rect::new(x, y, reach as u16, 1).intersection(clip);
            let mut cx = x;
            for (text, is_key) in runs {
                let fg = if is_key { t.text } else { t.text_muted };
                cx = put_str(
                    buf,
                    cx,
                    y,
                    &text,
                    Style::new().fg(fg).bg(t.background),
                    clip,
                );
            }
        }
        x += *w as u16 + 2;
    }
}

/// The message opencode puts after the spinner while a request retries: cut at 80 chars, with a
/// hint when the whole thing is over 120.
pub fn retry_text(msg: &str) -> String {
    let n = msg.chars().count();
    let shown: String = if n > 80 {
        msg.chars().take(80).chain(Some('…')).collect()
    } else {
        msg.to_string()
    };
    let hint = if n > 120 { " (click to expand)" } else { "" };
    format!("{shown}{hint}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn number_formats_like_locale() {
        assert_eq!(number(999), "999");
        assert_eq!(number(11_700), "11.7K");
        assert_eq!(number(2_500_000), "2.5M");
    }

    #[test]
    fn long_paths_wrap_in_rows() {
        let r = wrap_chars("/aaaa/bbbb/cccc", 6);
        assert_eq!(r, vec!["/aaaa/", "bbbb/", "cccc"]);
        let r = wrap_chars("/tmp/x-4f47-b09e-820c7c4f2577/proj", 20);
        assert_eq!(r, vec!["/tmp/x-4f47-b09e-", "820c7c4f2577/proj"]);
    }
}
