// OWNER: visual
//! Layout and the one `draw` that paints a frame. Regions from the top: transcript (the only
//! scrolling part), dock, composer (or the permission panel), status line. The right rail is
//! reserved from 140 columns up and an overlay below that.

pub mod blocks;
pub mod composer;
pub mod dialogs;
pub mod dock;
pub mod help;
pub mod links;
pub mod md;
pub mod permission;
pub mod popup;
pub mod rail;
pub mod resume;
pub mod row;
pub mod search;
pub mod select;
pub mod settings;
pub mod status;
pub mod tools;
pub mod transcript;
pub mod wrapcode;

use std::time::Instant;

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::Frame;
use tuikit::paint::{fill, put_str};
use tuikit::width::display_width;

use crate::app::{App, Focus, RailMode, SCROLLBAR_FADE};
use crate::ui::row::Cx;

/// Rows and columns of every region for one terminal size.
#[derive(Clone, Copy, Debug, Default)]
pub struct Geo {
    pub w: u16,
    pub h: u16,
    /// Main area width `A = W - R`.
    pub a: u16,
    /// Left offset `X` of the transcript column.
    pub x: u16,
    /// Column width `C`.
    pub c: u16,
    pub transcript: Rect,
    pub dock: Rect,
    pub composer: Rect,
    pub status: Rect,
    pub rail_rect: Option<Rect>,
    /// The rail is drawn over the transcript instead of beside it.
    pub rail_overlay: bool,
    /// The composer has its two padding rows.
    pub pad: bool,
    /// The composer has one padding row, above it only (20 to 29 rows).
    pub pad_top: bool,
}

impl Geo {
    /// Padding rows inside the composer rect, `(above, below)`.
    pub fn pads(&self) -> (u16, u16) {
        match (self.pad, self.pad_top) {
            (true, _) => (1, 1),
            (false, true) => (1, 0),
            _ => (0, 0),
        }
    }
}

/// Narrowest main area the transcript still reads well in once the rail takes its columns.
/// Below it (a toggled-on rail under 96 columns) the rail is drawn over the transcript.
const MIN_BESIDE_RAIL: u16 = 60;

pub fn geometry(w: u16, h: u16, rail: RailMode, dock: u16, comp: u16, panel: Option<u16>) -> Geo {
    let wide = w >= 140;
    let show_rail = match rail {
        RailMode::Auto => wide,
        RailMode::On => w >= 40,
        RailMode::Off => false,
    };
    // Finding 13: at 120 columns an overlay cut prose mid-word and hid the outcome column, so
    // a toggled-on rail takes its own columns whenever the rest still holds 60.
    let beside = show_rail && w.saturating_sub(rail::WIDTH) >= MIN_BESIDE_RAIL;
    let reserved = if beside { rail::WIDTH } else { 0 };
    let overlay = show_rail && !beside;
    let a = w - reserved;
    let (x, c) = if w < 40 {
        (0, a)
    } else if a.saturating_sub(4) < 100 {
        (2, a.saturating_sub(4))
    } else {
        ((a - 100) / 2, 100)
    };
    let status_h = u16::from(h >= 1);
    let avail = (h - status_h) as i32;
    let min_t: i32 = if h < 12 { 3 } else { 5 };
    // Two padding rows from 30 rows, one above the text from 20, so at 24 rows the last line
    // of an answer is not the row directly over the composer (finding 2).
    let mut pad_rows: i32 = match (panel.is_none(), h) {
        (true, 30..) => 2,
        (true, 20..) => 1,
        _ => 0,
    };
    let mut comp_total = match panel {
        Some(p) => p as i32,
        None => comp as i32 + pad_rows,
    };
    let mut dock = dock as i32;
    // Give rows back to the transcript in the stated order: dock, padding, composer rows.
    let short = |comp_total: i32, dock: i32| min_t - (avail - comp_total - dock);
    if short(comp_total, dock) > 0 {
        dock -= short(comp_total, dock).min(dock);
    }
    if short(comp_total, dock) > 0 && pad_rows > 0 {
        comp_total -= pad_rows;
        pad_rows = 0;
    }
    if short(comp_total, dock) > 0 {
        let floor = if panel.is_some() { 3 } else { 1 };
        comp_total -= short(comp_total, dock).min((comp_total - floor).max(0));
    }
    let t = (avail - comp_total - dock).max(0) as u16;
    let (comp_total, dock) = (comp_total as u16, dock as u16);
    let y_dock = t;
    let y_comp = t + dock;
    Geo {
        w,
        h,
        a,
        x,
        c,
        transcript: Rect::new(0, 0, a, t),
        dock: Rect::new(x, y_dock, c, dock),
        composer: Rect::new(x, y_comp, c, comp_total),
        status: Rect::new(0, h.saturating_sub(status_h), w, status_h),
        rail_rect: (show_rail && h > 1)
            .then(|| Rect::new(w - rail::WIDTH.min(w), 0, rail::WIDTH.min(w), avail as u16)),
        rail_overlay: overlay,
        pad: pad_rows == 2,
        pad_top: pad_rows == 1,
    }
}

/// The part of the status row the text uses. The mode word lands on the prose column
/// (`X + 2`), with the indicator slot to its left, so chrome and content share one grid
/// (finding 20). The right group stays on the right edge, or on the content column's edge once
/// the column is centred a long way in (`X > 12`).
pub fn status_region(geo: &Geo) -> Rect {
    let s = geo.status;
    let slot = status::slot_width(geo.w as usize) as u16;
    let x0 = geo.x.saturating_sub(slot);
    let right = if geo.x > 12 && geo.c > 0 {
        (geo.x + geo.c + 2).min(s.right())
    } else {
        s.right()
    };
    Rect::new(s.x + x0, s.y, right.saturating_sub(x0), 1)
}

/// A user message is `raised`, and so is the composer: when the last transcript row is a user
/// row the two read as one slab (round 2 finding 15: `turn 164` over the composer at 80x24).
/// Then the composer's first padding row becomes an edge, a lower half block in the raised
/// colour on the page colour, the way the dock has one: half a row of page between them.
fn raised_edge(
    buf: &mut ratatui::buffer::Buffer,
    geo: &Geo,
    p: &crate::palette::Palette,
    g: &crate::palette::Glyphs,
) {
    use crate::palette::Depth;
    let (up, _) = geo.pads();
    if up == 0
        || geo.dock.height > 0
        || geo.transcript.height == 0
        || !matches!(p.depth, Depth::True | Depth::Ansi256)
        || g.edge.is_empty()
    {
        return;
    }
    let above = geo.transcript.bottom() - 1;
    let x = geo.x + geo.c.saturating_sub(1);
    if buf[(x, above)].bg != p.raised {
        return;
    }
    let row = Rect::new(geo.composer.x, geo.composer.y, geo.composer.width, 1);
    fill(buf, row, Style::new().bg(p.bg));
    for dx in 0..row.width {
        put_str(
            buf,
            row.x + dx,
            row.y,
            g.edge,
            Style::new().fg(p.raised).bg(p.bg),
            row,
        );
    }
}

pub fn draw(app: &mut App, f: &mut Frame, now: Instant) {
    let area = f.area();
    let (w, h) = (area.width, area.height);
    app.size = (w, h);
    if w == 0 || h == 0 {
        return;
    }
    let spin = app.spin_index(now);
    if let Some(v) = app.diffview.as_mut() {
        // The diff viewer owns the whole screen.
        let cx = Cx {
            p: &app.p,
            theme: &app.theme,
            g: &app.g,
            width: w as usize,
            detail: app.detail,
            now,
            spin,
        };
        fill(
            f.buffer_mut(),
            area,
            Style::new().bg(app.p.bg).fg(app.p.text),
        );
        v.draw(f.buffer_mut(), area, &cx);
        app.p.downgrade(f.buffer_mut());
        app.last_draw = now;
        app.dirty = false;
        return;
    }
    // First pass for the column width, which decides how tall the composer and dock are.
    let base = geometry(w, h, app.rail, 0, 1, None);
    let comp_rows = app.composer.content_rows(base.c.saturating_sub(3));
    let dock = {
        let open = app
            .todos_open
            .unwrap_or(app.size.1 >= 30 && base.rail_rect.is_none());
        composer::dock_rows(
            &app.todos,
            open,
            &app.queue,
            app.busy(),
            base.c as usize,
            &app.p,
            &app.g,
        )
    };
    let panel_h = {
        let cx = Cx {
            p: &app.p,
            theme: &app.theme,
            g: &app.g,
            width: base.c as usize,
            detail: app.detail,
            now,
            spin,
        };
        app.perm
            .as_ref()
            .map(|p| p.height(&cx, h as usize))
            .or_else(|| app.rewind.as_ref().map(|r| r.height()))
    };
    // The edge row above the tray counts with it.
    let dock_h = if dock.is_empty() {
        0
    } else {
        dock.len() as u16 + 1
    };
    let geo = geometry(w, h, app.rail, dock_h, comp_rows, panel_h);
    app.geo = geo;

    let buf = f.buffer_mut();
    fill(buf, area, Style::new().bg(app.p.bg).fg(app.p.text));
    let cx = Cx {
        p: &app.p,
        theme: &app.theme,
        g: &app.g,
        width: geo.c as usize,
        detail: app.detail,
        now,
        spin,
    };

    // Transcript.
    let vh = geo.transcript.height as usize;
    let frame = app.tr.frame(vh, &mut app.view, &cx);
    app.tr
        .paint(buf, geo.transcript, geo.x, geo.c, &frame, &app.view, &cx);
    // The thumb shows for 1.5 s after scroll activity, and for as long as the view is not
    // following the end: a person reading page 12 of 200 has nothing else saying where they
    // are (round 2 finding 15).
    let active = app
        .view
        .scrolled_at
        .is_some_and(|t| now.saturating_duration_since(t) < SCROLLBAR_FADE);
    if (active || !app.view.sticky) && vh > 0 {
        let (above, total) = app.tr.extent(&app.view, vh, geo.c as usize);
        if total > vh {
            let len = ((vh * vh) / total).clamp(1, vh);
            let start = (above * (vh - len)) / (total - vh).max(1);
            let x = geo.a.saturating_sub(1);
            for r in 0..len {
                put_str(
                    buf,
                    x,
                    (start + r) as u16,
                    "┃",
                    Style::new().fg(app.p.faint).bg(app.p.bg),
                    geo.transcript,
                );
            }
        }
    }
    if !app.view.sticky && app.tr.rev != app.view.rev_at_unstick && vh > 0 {
        let label = format!(" {} new output ", app.g.down);
        let lw = display_width(&label) as u16;
        let x = (geo.x + geo.c).saturating_sub(lw);
        put_str(
            buf,
            x,
            geo.transcript.bottom() - 1,
            &label,
            Style::new().fg(app.p.accent).bg(app.p.bg),
            geo.transcript,
        );
    }

    // Dock. State, not content: its own tray under an edge row (finding 2).
    dock::paint(buf, geo.dock, &dock, &app.p, &app.g);

    // Composer or permission panel.
    let mut cursor = None;
    if let Some(panel) = app.perm.as_mut() {
        panel.queued = app.perm_queue.len();
        panel.draft.clear();
        panel.draft.push_str(app.composer.text());
        cursor = panel.draw(buf, geo.composer, &cx);
    } else if let Some(r) = app.rewind.as_ref() {
        r.draw(buf, geo.composer, &cx);
    } else {
        let (placeholder, hint) = composer::placeholder(app.busy(), app.queue.len(), geo.c);
        let mut comp_rect = geo.composer;
        if geo.pad_top && comp_rect.height > 1 {
            // One padding row above the text: the composer draws its own two, so this one is
            // painted here and the composer gets the rest.
            let top = Rect::new(comp_rect.x, comp_rect.y, comp_rect.width, 1);
            fill(buf, top, Style::new().bg(app.p.raised));
            let bar = Style::new()
                .fg(if app.focus == Focus::Composer {
                    app.p.user
                } else {
                    app.p.line
                })
                .bg(app.p.raised);
            put_str(buf, top.x, top.y, app.g.bar, bar, top);
            comp_rect = Rect::new(
                comp_rect.x,
                comp_rect.y + 1,
                comp_rect.width,
                comp_rect.height - 1,
            );
        }
        let c = app.composer.render(
            buf,
            comp_rect,
            geo.pad,
            app.focus == Focus::Composer,
            &placeholder,
            &hint,
            &app.p,
            &app.g,
        );
        if app.focus == Focus::Composer {
            cursor = c;
        }
    }
    if app.perm.is_none() && app.rewind.is_none() {
        raised_edge(buf, &geo, &app.p, &app.g);
    }
    if app.perm.is_none() && app.rewind.is_none() && app.focus == Focus::Composer {
        if let Some(pop) = &app.composer.popup {
            // One or two rows go over the composer's own padding row rather than over the
            // transcript above it (finding 29: the `@` popup ate half the welcome).
            let rows = pop.items.len().min((h / 3).clamp(1, 8) as usize);
            let (up, _) = geo.pads();
            let lift = if rows < 3 { up } else { 0 };
            let place = composer::PopupPlace {
                x: geo.x,
                c: geo.c,
                bottom: geo.composer.y + lift,
                max_rows: (h / 3).clamp(1, 8),
            };
            composer::draw_popup(buf, pop, place, &app.p, &app.g);
        }
    }

    // Status line, or the search prompt.
    let search_counts = app.tr.search_counts();
    let search_approx = app.tr.search_approximate();
    if let Some(ed) = app.search_ed.as_mut() {
        let r = geo.status;
        fill(buf, r, Style::new().bg(app.p.bg));
        put_str(buf, r.x + 2, r.y, "/", Style::new().fg(app.p.accent), r);
        // The count sits at the right end so it can be read while typing.
        let count_w = match search_counts {
            Some((cur, total)) => {
                let (label, st) = if total == 0 {
                    ("no match".to_string(), Style::new().fg(app.p.warn))
                } else {
                    // `+`: blocks not laid out yet count as one each and may hold more or none.
                    let plus = if search_approx { "+" } else { "" };
                    (
                        format!("{cur}/{total}{plus}"),
                        Style::new().fg(app.p.accent),
                    )
                };
                let lw = display_width(&label) as u16;
                put_str(buf, r.right().saturating_sub(lw + 2), r.y, &label, st, r);
                lw + 2
            }
            None => 0,
        };
        let st = tuikit::editor::EditorStyle {
            text: Style::new().fg(app.p.text),
            placeholder: Some(("search the transcript".into(), Style::new().fg(app.p.faint))),
        };
        let area = Rect::new(r.x + 3, r.y, r.width.saturating_sub(5 + count_w), 1);
        cursor = ed.render(area, buf, &st).cursor;
    } else {
        let info = app.status_info(now);
        // Finding 20: on a wide terminal the content column is centred and the status text
        // used to sit 30 cells to its left. From X > 12 it shares the content column's grid.
        let region = status_region(&geo);
        let spans = status::line(&info, region.width as usize, &app.p, &app.g);
        let mut x = region.x;
        for s in &spans {
            x = put_str(
                buf,
                x,
                geo.status.y,
                &s.content,
                Style::new().bg(app.p.bg).patch(s.style),
                region,
            );
        }
    }

    // Rail.
    if let Some(r) = geo.rail_rect {
        let info = rail::RailInfo {
            title: app.session_title(),
            cwd: app.cwd_str(),
            ctx_tokens: app.tr.usage.context_tokens,
            ctx_window: app.tr.usage.context_window,
            cost: app.tr.usage.cost_usd,
            turns: app.tr.turns,
            changes: app.tr.changes().to_vec(),
            todos: app.todos.clone(),
            agents: app.tr.running_agents(),
        };
        rail::draw(buf, r, &info, &app.p, &app.g);
    }

    // Overlays.
    let sv = app.settings_view();
    if let Some(ov) = app.overlay.as_mut() {
        let octx = dialogs::OvCtx {
            p: &app.p,
            theme: &app.theme,
            g: &app.g,
            settings: &sv,
        };
        let c = ov.draw(buf, area, &octx);
        cursor = c;
    }

    links::apply(buf);
    app.p.downgrade(buf);
    if let Some((x, y)) = cursor {
        f.set_cursor_position((x, y));
    }
    app.last_draw = now;
    app.dirty = false;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn column_geometry_at_the_design_sizes() {
        // 80x24: C = 76, X = 2, one padding row above the composer text.
        let g = geometry(80, 24, RailMode::Auto, 1, 1, None);
        assert_eq!((g.x, g.c, g.a), (2, 76, 80));
        assert!(!g.pad && g.pad_top && g.rail_rect.is_none());
        assert_eq!(g.composer.height, 2);
        assert_eq!(g.transcript.height, 24 - 1 - 2 - 1);
        // 120x36: C = 100, X = 10, padded composer, rows H-4.
        let g = geometry(120, 36, RailMode::Auto, 0, 1, None);
        assert_eq!((g.x, g.c), (10, 100));
        assert!(g.pad);
        assert_eq!(g.composer.y, 36 - 4);
        assert_eq!(g.composer.height, 3);
        assert_eq!(g.dock.height, 0);
        // 160x45: R = 36, A = 124, X = 12.
        let g = geometry(160, 45, RailMode::Auto, 1, 1, None);
        assert_eq!((g.a, g.x, g.c), (124, 12, 100));
        let r = g.rail_rect.unwrap();
        assert_eq!((r.x, r.width), (124, 36));
        assert_eq!(g.status.width, 160);
    }

    #[test]
    fn prose_never_exceeds_100_at_200_columns() {
        let g = geometry(200, 50, RailMode::Auto, 0, 1, None);
        assert_eq!(g.c, 100);
        assert_eq!(g.x, (200 - 36 - 100) / 2);
        let g = geometry(240, 50, RailMode::Off, 0, 1, None);
        assert_eq!(g.c, 100);
    }

    #[test]
    fn transcript_never_drops_below_five_rows_while_there_is_room() {
        // A tall composer and a full dock give way: dock first, then padding, then rows.
        let g = geometry(100, 20, RailMode::Auto, 7, 8, None);
        assert!(g.transcript.height >= 5, "{g:?}");
        assert_eq!(
            g.dock.height + g.composer.height + g.transcript.height + 1,
            20
        );
        let g = geometry(100, 10, RailMode::Auto, 7, 8, None);
        assert!(g.transcript.height >= 3, "{g:?}");
    }

    #[test]
    fn rail_is_an_overlay_below_140_and_absent_below_40() {
        let g = geometry(90, 30, RailMode::On, 0, 1, None);
        assert!(g.rail_overlay && g.rail_rect.is_some());
        assert_eq!(g.a, 90);
        let g = geometry(30, 30, RailMode::On, 0, 1, None);
        assert!(g.rail_rect.is_none());
        assert_eq!(g.x, 0);
    }

    #[test]
    fn a_toggled_on_rail_takes_columns_instead_of_covering_the_prose() {
        // Finding 13: at 120 the overlay cut `follow up in not` mid-word and hid every outcome.
        let g = geometry(120, 36, RailMode::On, 0, 1, None);
        assert!(!g.rail_overlay);
        assert_eq!((g.a, g.c, g.x), (84, 80, 2));
        let r = g.rail_rect.unwrap();
        assert_eq!((r.x, r.width), (84, 36));
        assert!(g.x + g.c <= r.x, "the column ends before the rail starts");
        // Below 96 columns the column would be under 60 wide, so it is the overlay again.
        assert!(geometry(95, 30, RailMode::On, 0, 1, None).rail_overlay);
        assert!(!geometry(96, 30, RailMode::On, 0, 1, None).rail_overlay);
    }

    #[test]
    fn the_composer_has_padding_from_20_rows_and_gives_it_up_first() {
        for (h, rows) in [(19, 0), (20, 1), (29, 1), (30, 2), (45, 2)] {
            let g = geometry(100, h, RailMode::Auto, 0, 1, None);
            assert_eq!(g.composer.height, 1 + rows, "{h} rows");
            let (up, down) = g.pads();
            assert_eq!(up + down, rows, "{h} rows");
        }
        // A tight screen drops the padding before it touches the 5 transcript rows.
        let g = geometry(100, 12, RailMode::Auto, 0, 3, None);
        assert!(g.transcript.height >= 5 && g.pads() == (0, 0), "{g:?}");
    }

    #[test]
    fn the_mode_word_lands_on_the_prose_column_at_every_wide_size() {
        use crate::ui::status::slot_width;
        for (w, h) in [(120u16, 36u16), (160, 45), (200, 55), (240, 60)] {
            let g = geometry(w, h, RailMode::Auto, 0, 1, None);
            let r = status_region(&g);
            // 2 cells of padding and the slot come before the mode word.
            assert_eq!(
                r.x + 2 + slot_width(r.width as usize) as u16,
                g.x + 2,
                "{w}: mode word on the prose column"
            );
            if g.x > 12 {
                assert_eq!(r.right() - 2, g.x + g.c, "{w}: ends at the column's edge");
            } else {
                assert_eq!(r.right(), w, "{w}: runs to the right edge");
            }
        }
        // Where X is smaller than the slot the row starts at the edge.
        let g = geometry(80, 24, RailMode::Auto, 0, 1, None);
        assert_eq!(status_region(&g).x, 0);
    }
}
