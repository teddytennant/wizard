// OWNER: composer
//! The prompt box and the popup above it. The box is a rounded border with the prefix and text
//! inside and the model label set into the bottom border; the popup (slash commands, an argument
//! list or the `@` file picker) is a panel of up to eight lines between two rules directly above
//! the box. Chips, the paste and image previews and the history panel live in the submodules.

pub mod at;
pub mod fuzz;
pub mod hist;
pub mod input;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use tuikit::editor::EditorStyle;
use tuikit::paint::put_str;
use tuikit::width::{display_width, truncate};

use super::{bold, put, st, Layout};
use crate::app::{App, Focus, Mode, Screen};
use crate::theme::blend;

/// Text rows the composer wants, between 1 and `max`.
pub fn text_rows(app: &App, comp_w: u16, max: u16) -> u16 {
    // browsing history freezes the box at one row; the entry scrolls inside it
    if app.inp.hist.is_some() {
        return 1;
    }
    let w = comp_w.saturating_sub(6).max(1);
    let needed = app.ed.needed_height(w);
    // an unfocused box collapses to three rows
    let cap = if app.screen == Screen::Session && app.focus != Focus::Prompt {
        3.min(max.max(1))
    } else {
        max.max(1)
    };
    needed.clamp(1, cap)
}

pub fn text_width(comp_w: u16) -> u16 {
    comp_w.saturating_sub(6).max(1)
}

fn border_color(app: &App, focused: bool) -> Color {
    let th = &app.theme;
    if !focused {
        th.prompt_border
    } else if app.mode == Mode::Plan && !app.shell_mode {
        blend(th.bg_base, th.accent_plan, 0.4)
    } else {
        th.prompt_border_active
    }
}

/// Whether the prompt looks focused: always on the agent screen unless the scrollback has the
/// keyboard, and on home until Esc.
pub fn focused(app: &App) -> bool {
    match app.screen {
        Screen::Home => !app.home.unfocused,
        Screen::Session => app.focus == Focus::Prompt,
    }
}

/// The label set into the bottom border as `(text, style)` runs.
fn label_runs(app: &App, focused: bool) -> Vec<(String, Style)> {
    let th = &app.theme;
    let a = if focused { 0.6 } else { 0.4 };
    let main = th.recede(th.text_secondary, a);
    if app.shell_mode {
        return vec![("Run shell command".into(), st(main))];
    }
    let mut v = vec![(app.model_label(), st(main))];
    if app.mode == Mode::Plan {
        let k = if focused { 0.75 } else { 0.5 };
        v.push((" · ".into(), st(th.gray_dim)));
        v.push(("plan".into(), st(blend(th.bg_base, th.accent_plan, k))));
    }
    if app.multiline {
        v.push((" ─".into(), st(border_color(app, focused))));
        v.push(("multiline".into(), st(th.gray)));
    }
    v
}

/// The caption in the top border: `Stashed`, the `/rename` title, or both.
fn caption(app: &App) -> Option<String> {
    let title = app
        .titles
        .get(&app.tr.session_id)
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());
    match (app.stash.is_some(), title) {
        (true, Some(t)) => Some(format!("Stashed · {t}")),
        (true, None) => Some("Stashed".into()),
        (false, t) => t,
    }
}

pub fn draw(buf: &mut Buffer, app: &mut App, lay: &Layout) {
    let th = app.theme.clone();
    let focused = focused(app);
    let r = lay.composer;
    let bc = border_color(app, focused);
    let bs = st(bc);
    let right = r.x + r.width.saturating_sub(1);
    let bottom = r.y + r.height.saturating_sub(1);
    // top border, with the caption near the right corner
    let top = format!("╭{}╮", "─".repeat(r.width.saturating_sub(2) as usize));
    put(buf, r.x, r.y, &top, bs);
    if let Some(cap) = caption(app) {
        let max_w = r.width.saturating_sub(6) as usize;
        if max_w >= 6 {
            let label = truncate(&format!(" {cap} "), max_w);
            let w = display_width(&label) as u16;
            put(
                buf,
                right.saturating_sub(2 + w),
                r.y,
                &label,
                st(th.recede(th.text_secondary, if focused { 0.6 } else { 0.4 })),
            );
        }
    }
    // bottom border with the label
    let runs = label_runs(app, focused);
    let label_w: usize = runs.iter().map(|(t, _)| display_width(t)).sum();
    let tail_w = label_w + 4; // " " label " ─╯"
    let fill = (r.width as usize).saturating_sub(1 + tail_w);
    let mut line = String::from("╰");
    line.push_str(&"─".repeat(fill));
    put(buf, r.x, bottom, &line, bs);
    let mut x = r.x + 1 + fill as u16;
    put(buf, x, bottom, " ", bs);
    x += 1;
    for (t, s) in &runs {
        x = put(buf, x, bottom, t, *s);
    }
    put(buf, x, bottom, " ─╯", bs);
    // sides
    for y in r.y + 1..bottom {
        put(buf, r.x, y, "│", bs);
        put(buf, right, y, "│", bs);
    }
    // prefix and text
    let tx = r.x + 4;
    let tw = text_width(r.width);
    let text_area = Rect::new(tx, r.y + 1, tw, r.height.saturating_sub(2));
    let search = app.inp.hist.as_ref().is_some_and(|p| !p.browse);
    let (prefix, pc) = if search {
        ("? ", th.accent_user)
    } else if app.shell_mode {
        ("! ", th.command)
    } else if app.mode == Mode::Plan {
        ("❯ ", th.accent_plan)
    } else {
        ("❯ ", th.accent_user)
    };
    let pc = if focused {
        pc
    } else {
        th.recede(th.gray_dim, 0.66)
    };
    put(buf, r.x + 2, r.y + 1, prefix, st(pc));
    let text_color = if focused {
        th.text_primary
    } else {
        th.recede(th.text_primary, 0.66)
    };

    let empty = app.ed.is_empty();
    if empty && !focused {
        let ph = match app.screen {
            Screen::Home => "Type a message...",
            Screen::Session => "Build anything",
        };
        put(buf, tx, r.y + 1, ph, st(th.placeholder));
        app.cursor = None;
        return;
    }
    let popup = popup(app);
    let single = !app.ed.text().contains('\n');
    if focused && single && app.ed.text().starts_with('/') && !app.shell_mode {
        draw_slash_text(buf, app, popup.as_ref(), tx, r.y + 1, tw);
        return;
    }
    let style = EditorStyle {
        text: st(text_color),
        placeholder: None,
    };
    // a box that overflows gives its last text column to the scrollbar, and the text wraps
    // one cell narrower
    let overflow = app.ed.needed_height(tw) > text_area.height;
    let wrap_area = if overflow {
        Rect::new(tx, text_area.y, tw - 1, text_area.height)
    } else {
        text_area
    };
    let info = app.ed.render(wrap_area, buf, &style);
    app.cursor = if focused { info.cursor } else { None };
    paint_tokens(buf, app, wrap_area, focused);
    // a textarea that overflows its box shows a thumb in the last text column
    if info.needed_height > text_area.height && text_area.height > 0 {
        let col = tx + tw - 1;
        let n = info.needed_height as usize;
        let h = text_area.height as usize;
        let top = app.ed.scroll();
        if let Some((pos, size)) = super::thumb(n, h, top, h) {
            for i in pos..(pos + size).min(h) {
                put(
                    buf,
                    col,
                    text_area.y + i as u16,
                    "█",
                    st(th.selection_border),
                );
            }
        }
    }
    if focused {
        if let Some(p) = popup {
            draw_popup(buf, app, &p, lay);
        }
        hist::draw(buf, app, lay);
        draw_previews(buf, app, lay);
    }
}

/// Paint the chips and the `@path` references over the text the editor drew.
fn paint_tokens(buf: &mut Buffer, app: &App, area: Rect, focused: bool) {
    let th = &app.theme;
    let text = app.ed.text();
    let mut runs: Vec<(std::ops::Range<usize>, Style)> = Vec::new();
    // chips: brackets dim, label bright, all on the chip background
    let chip_bg = th.paste_bg;
    for r in input::spans(text, &app.inp.chips).into_iter().flatten() {
        let dim = Style::new().fg(th.paste_dim).bg(chip_bg);
        let lab = Style::new().fg(th.paste_fg).bg(chip_bg);
        runs.push((r.start..r.start + 1, dim));
        runs.push((r.start + 1..r.end - 1, lab));
        runs.push((r.end - 1..r.end, dim));
    }
    // `@path`, `@path:12-30`: only for a path that is there
    let mut i = 0;
    while let Some(off) = text[i..].find('@') {
        let at = i + off;
        i = at + 1;
        if text[..at]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_alphanumeric() || c == '_')
        {
            continue;
        }
        let end = text[at + 1..]
            .char_indices()
            .find(|(_, c)| c.is_whitespace() || matches!(c, ',' | ';'))
            .map_or(text.len(), |(k, _)| at + 1 + k);
        let tok = &text[at + 1..end];
        let (path, range) = match tok.split_once(':') {
            Some((p, r)) => (p, Some(r)),
            None => (tok, None),
        };
        if path.is_empty() || !path_exists(app, path) {
            continue;
        }
        let g = |c| Style::new().fg(c);
        runs.push((at..at + 1, g(th.gray)));
        runs.push((at + 1..at + 1 + path.len(), g(th.path)));
        if range.is_some() {
            runs.push((at + 1 + path.len()..end, g(th.gray_bright)));
        }
        i = end;
    }
    if runs.is_empty() {
        return;
    }
    let rows = app.ed.layout(area.width as usize);
    let scroll = app.ed.scroll();
    for (range, style) in runs {
        for (ri, row) in rows.iter().enumerate() {
            if ri < scroll || ri >= scroll + area.height as usize {
                continue;
            }
            let (s, e) = (range.start.max(row.start), range.end.min(row.end));
            if s >= e {
                continue;
            }
            let x = area.x + display_width(&text[row.start..s]) as u16;
            let y = area.y + (ri - scroll) as u16;
            let style = if focused {
                style
            } else {
                // the unfocused box dims everything inside it
                match style.fg {
                    Some(c) => style.fg(blend(th.bg_base, c, 0.66)),
                    None => style,
                }
            };
            put_str(buf, x, y, &text[s..e], style, area);
        }
    }
}

fn path_exists(app: &App, path: &str) -> bool {
    let p = path.trim_end_matches('/');
    app.files.iter().any(|f| f.trim_end_matches('/') == p) || app.opts.cwd.join(p).exists()
}

/// The paste preview over the scrollback while the cursor is on or right after a paste chip, or
/// the image card for an image chip.
fn draw_previews(buf: &mut Buffer, app: &App, lay: &Layout) {
    let Some(i) = input::chip_near(app)
        .map(|(i, _)| i)
        .or_else(|| input::image_near(app))
    else {
        return;
    };
    let chip = &app.inp.chips[i];
    let th = &app.theme;
    // the area is the scrollback above the box, down to the row above the first strip
    let area = Rect::new(
        lay.hpad,
        lay.view.y,
        lay.w.saturating_sub(2 * lay.hpad),
        lay.view.bottom().saturating_sub(lay.view.y),
    );
    if chip.image {
        draw_image_card(buf, app, area, chip);
        return;
    }
    let lines: Vec<&str> = chip.content.lines().collect();
    let total = lines.len();
    if total == 0 || area.height < 5 || area.width < 20 {
        return;
    }
    let dots = total > 6;
    let content_rows = if dots { 7 } else { total };
    let bh = (content_rows as u16 + 2).min(area.height);
    let bw = (area.width as f32 * 0.75) as u16;
    let bx = area.x + area.width.saturating_sub(bw) / 2;
    let by = (area.y + area.height).saturating_sub(bh);
    let bg = th.paste_bg;
    let box_ = Rect::new(bx, by, bw, bh);
    super::fill(buf, box_, bg);
    let bd = Style::new().fg(th.paste_dim).bg(bg);
    put(
        buf,
        bx,
        by,
        &format!("╭{}╮", "─".repeat(bw as usize - 2)),
        bd,
    );
    put(
        buf,
        bx,
        by + bh - 1,
        &format!("╰{}╯", "─".repeat(bw as usize - 2)),
        bd,
    );
    for y in by + 1..by + bh - 1 {
        put(buf, bx, y, "│", bd);
        put(buf, bx + bw - 1, y, "│", bd);
    }
    let inner = Rect::new(bx + 1, by + 1, bw - 2, bh - 2);
    let text = Style::new().fg(th.paste_fg).bg(bg);
    let mut row = 0u16;
    let mut line = |s: String, style: Style, row: &mut u16| {
        if *row < inner.height {
            let t = truncate(&s, inner.width as usize);
            put_str(buf, inner.x, inner.y + *row, &t, style, inner);
            *row += 1;
        }
    };
    if dots {
        for l in lines.iter().take(3) {
            line((*l).to_string(), text, &mut row);
        }
        line(format!("⋮ ({} more lines)", total - 6), bd, &mut row);
        for l in lines.iter().skip(total - 3) {
            line((*l).to_string(), text, &mut row);
        }
    } else {
        for l in &lines {
            line((*l).to_string(), text, &mut row);
        }
    }
    // the hint sits in the bottom border, after the corner and one dash
    let on = input::chip_at(app).is_some();
    let act = if on { "enter" } else { "paste again" };
    if (bw as usize).saturating_sub(6) >= 8 {
        let hx = bx + 2;
        let hb = Style::new().bg(bg);
        let chord = Style::new()
            .fg(th.fuzzy_accent)
            .bg(bg)
            .add_modifier(Modifier::BOLD);
        let mut x = put(buf, hx, by + bh - 1, " ", hb);
        x = put(buf, x, by + bh - 1, act, chord);
        x = put(buf, x, by + bh - 1, " or ", Style::new().fg(th.gray).bg(bg));
        x = put(buf, x, by + bh - 1, "double-click", chord);
        x = put(
            buf,
            x,
            by + bh - 1,
            " to expand",
            Style::new().fg(th.gray).bg(bg),
        );
        put(buf, x, by + bh - 1, " ", hb);
    }
}

/// An image chip's card. grokw has no way to draw pixels, so it is the metadata form: format,
/// size and path, bottom aligned over the scrollback.
fn draw_image_card(buf: &mut Buffer, app: &App, area: Rect, chip: &input::Chip) {
    let th = &app.theme;
    if area.width < 28 || area.height < 6 {
        return;
    }
    // everything behind it recedes
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            if let Some(c) = buf.cell_mut((x, y)) {
                c.modifier = Modifier::empty();
                if let Color::Rgb(..) = c.fg {
                    c.fg = blend(th.bg_base, c.fg, 0.5);
                }
            }
        }
    }
    let bw = (((area.width as f32) * 0.75) as u16).clamp(28, area.width);
    let bh = 6u16.min(area.height);
    let bx = area.x + area.width.saturating_sub(bw) / 2;
    let by = area.y + area.height.saturating_sub(bh);
    let bg = th.paste_bg;
    super::fill(buf, Rect::new(bx, by, bw, bh), bg);
    let bd = Style::new().fg(th.paste_dim).bg(bg);
    put(
        buf,
        bx,
        by,
        &format!("╭{}╮", "─".repeat(bw as usize - 2)),
        bd,
    );
    put(
        buf,
        bx,
        by + bh - 1,
        &format!("╰{}╯", "─".repeat(bw as usize - 2)),
        bd,
    );
    for y in by + 1..by + bh - 1 {
        put(buf, bx, y, "│", bd);
        put(buf, bx + bw - 1, y, "│", bd);
    }
    let text = Style::new().fg(th.paste_fg).bg(bg);
    let n = chip.label.trim_start_matches('[').trim_end_matches(']');
    let info = chip.info.clone();
    let fmt = info
        .as_ref()
        .map_or("PNG".to_string(), |i| mime_name(&i.mime));
    let name = std::path::Path::new(&chip.content)
        .file_name()
        .map(|n| n.to_string_lossy().to_string());
    let mut meta = vec![fmt.clone()];
    if let Some((w, h)) = info.as_ref().and_then(|i| i.dims) {
        meta.push(format!("{w}x{h}"));
    }
    if let Some(i) = &info {
        meta.push(format_bytes(i.bytes));
    }
    if let Some(nm) = name {
        meta.push(nm);
    }
    let title = format!(" {n} ");
    let full = format!("{title}─ {} ", meta.join(" · "));
    let full = if display_width(&full) + 6 < bw as usize {
        full
    } else {
        title
    };
    let tx = bx + (bw.saturating_sub(display_width(&full) as u16)) / 2;
    put(
        buf,
        tx,
        by,
        &full,
        Style::new()
            .fg(th.paste_fg)
            .bg(bg)
            .add_modifier(Modifier::BOLD),
    );
    let inner = Rect::new(bx + 1, by + 1, bw - 2, bh - 2);
    let mut lines = vec![format!("Format: {fmt}")];
    if let Some((w, h)) = info.as_ref().and_then(|i| i.dims) {
        lines.push(format!("Dimensions: {w} x {h}"));
    }
    if let Some(i) = &info {
        lines.push(format!("Size: {}", format_bytes(i.bytes)));
    }
    for (k, l) in lines.iter().enumerate().take(inner.height as usize - 1) {
        put_str(buf, inner.x, inner.y + k as u16, l, text, inner);
    }
    let path = format!(
        "Path: {}",
        mid_truncate(&chip.content, inner.width.saturating_sub(6) as usize)
    );
    put_str(buf, inner.x, inner.y + inner.height - 1, &path, text, inner);
}

fn mime_name(m: &str) -> String {
    match m {
        "image/png" => "PNG".into(),
        "image/jpeg" => "JPEG".into(),
        "image/tiff" => "TIFF".into(),
        "image/gif" => "GIF".into(),
        "image/webp" => "WebP".into(),
        "image/bmp" => "BMP".into(),
        o => o.into(),
    }
}

fn format_bytes(n: u64) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{:.1} KB", n as f64 / 1024.0)
    } else {
        format!("{:.1} MB", n as f64 / 1048576.0)
    }
}

/// A long path cut in the middle with `...`.
fn mid_truncate(p: &str, max: usize) -> String {
    let c: Vec<char> = p.chars().collect();
    if c.len() <= max || max <= 3 {
        return p.chars().take(max).collect();
    }
    let keep = (max - 3) / 2;
    let end_keep = max - 3 - keep;
    let head: String = c[..keep].iter().collect();
    let tail: String = c[c.len() - end_keep..].iter().collect();
    format!("{head}...{tail}")
}

/// `/name` coloured as a command while the popup is open, with the ghost of the selected row.
fn draw_slash_text(buf: &mut Buffer, app: &mut App, popup: Option<&Popup>, x: u16, y: u16, w: u16) {
    let th = app.theme.clone();
    let text = app.ed.text().to_string();
    let clip = Rect::new(x, y, w, 1);
    let (token, rest) = match text.find(char::is_whitespace) {
        Some(i) => (&text[..i], &text[i..]),
        None => (text.as_str(), ""),
    };
    let known = popup.is_some();
    let tok_style = if known {
        st(th.fuzzy_accent)
    } else {
        st(th.text_primary)
    };
    let mut cx = put_str(buf, x, y, token, tok_style, clip);
    cx = put_str(buf, cx, y, rest, st(th.text_primary), clip);
    // ghost: the untyped suffix of the highlighted command, or the argument hint
    if let Some(p) = popup {
        match p.kind {
            PopupKind::Slash => {
                if let Some(it) = p
                    .items
                    .get(app.popup_sel.min(p.items.len().saturating_sub(1)))
                {
                    if let Some(suffix) = it.insert.strip_prefix(token) {
                        let suffix = suffix.trim_end();
                        put_str(buf, cx, y, suffix, st(th.gray), clip);
                    }
                }
            }
            PopupKind::Arg | PopupKind::At => {}
        }
    }
    if rest.trim().is_empty() && rest.starts_with(' ') {
        if let Some(h) = crate::commands::hint_for(app, token.trim_start_matches('/')) {
            put_str(buf, cx, y, &h, st(th.gray), clip);
        }
    }
    app.cursor = Some((x + display_width(&text) as u16, y));
    if let Some(p) = popup {
        let lay = Layout::compute(app);
        draw_popup(buf, app, p, &lay);
    }
}

// ---- popup ----------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PopupKind {
    Slash,
    Arg,
    /// `@` file picker: the count reads `matches/files`, Up and Down stop at the ends, Enter never sends.
    At,
}

#[derive(Clone, Debug)]
pub struct PopupItem {
    /// Shown label (`/model`, `Grok 4.7 (current)`).
    pub label: String,
    pub tag: Option<String>,
    pub desc: String,
    /// What Tab or Enter puts in the composer.
    pub insert: String,
    /// Char indices of `label` that matched the query.
    pub hits: Vec<usize>,
    /// Byte range of the composer text `insert` replaces; `None` replaces all of it.
    pub range: Option<(usize, usize)>,
    /// A directory (`@` picker rows).
    pub dir: bool,
}

#[derive(Clone, Debug)]
pub struct Popup {
    pub kind: PopupKind,
    pub items: Vec<PopupItem>,
    /// Entries the `@` index holds, the denominator of its count.
    pub total: usize,
    /// The `@` query ends in `/`: rows are directories and show a trailing slash.
    pub dir_mode: bool,
}

/// The popup for the composer text, if one should be open.
pub fn popup(app: &App) -> Option<Popup> {
    if app.shell_mode
        || app.popup_closed
        || app.modal.is_some()
        || app.inp.hist.is_some()
        || !focused(app)
    {
        return None;
    }
    let text = app.ed.text();
    if !text.starts_with('/') || text.contains('\n') {
        return at_popup(app);
    }
    let body = &text[1..];
    match body.find(char::is_whitespace) {
        None => {
            if body.contains('/') {
                return None;
            }
            let items = crate::commands::slash_items(app, body);
            (!items.is_empty()).then_some(Popup {
                kind: PopupKind::Slash,
                items,
                total: 0,
                dir_mode: false,
            })
        }
        Some(i) => {
            let (cmd, arg) = (&body[..i], body[i..].trim_start());
            let items = crate::commands::arg_items(app, cmd, arg);
            (!items.is_empty()).then_some(Popup {
                kind: PopupKind::Arg,
                items,
                total: 0,
                dir_mode: false,
            })
        }
    }
}

/// The `@path` token under the cursor and the files that match it.
fn at_popup(app: &App) -> Option<Popup> {
    let text = app.ed.text();
    let cur = app.ed.cursor().min(text.len());
    let ctx = at::detect(text, cur)?;
    // a chip's own text is not a query
    if input::chip_near(app)
        .is_some_and(|(_, r)| r.start < ctx.range.end && ctx.range.start < r.end)
    {
        return None;
    }
    let found = at::search(app, &ctx);
    if found.items.is_empty() {
        return None;
    }
    let dir_mode = ctx.dir_mode();
    let items = found
        .items
        .into_iter()
        .map(|h| PopupItem {
            label: if dir_mode && h.is_dir {
                format!("{}/", h.path)
            } else {
                h.path.clone()
            },
            tag: None,
            desc: String::new(),
            insert: h.path,
            hits: h.hits.into_iter().map(|i| i as usize).collect(),
            range: Some((ctx.range.start, ctx.range.end)),
            dir: h.is_dir,
        })
        .collect();
    Some(Popup {
        kind: PopupKind::At,
        items,
        total: found.total,
        dir_mode,
    })
}

struct FlatLine {
    item: usize,
    first: bool,
    desc: String,
}

/// Wrap `s` at the last space within the first `width` chars, so a row that is not the last
/// holds at most `width - 1`; a description that fits in `width` stays on one row.
fn wrap_desc(s: &str, width: usize) -> Vec<String> {
    let mut rest: Vec<char> = s.trim().chars().collect();
    let mut out = Vec::new();
    if width < 4 {
        return vec![s.trim().to_string()];
    }
    while rest.len() > width {
        let cut = rest[..width]
            .iter()
            .rposition(|c| *c == ' ')
            .filter(|&i| i > 0)
            .unwrap_or(width - 1);
        out.push(
            rest[..cut]
                .iter()
                .collect::<String>()
                .trim_end()
                .to_string(),
        );
        let skip = if rest.get(cut) == Some(&' ') {
            cut + 1
        } else {
            cut
        };
        rest = rest[skip..].to_vec();
    }
    out.push(rest.into_iter().collect());
    out
}

/// Where a popup's rows are: shared by the drawing and by the mouse.
struct Geo {
    x0: u16,
    w: u16,
    item_x: u16,
    full_w: usize,
    col: usize,
    sel: usize,
    lines: Vec<FlatLine>,
    visible: usize,
    scrolled: bool,
    first_line: usize,
    rule_top: u16,
}

fn geometry(app: &App, p: &Popup, lay: &Layout) -> Option<Geo> {
    let total_items = p.items.len();
    let x0 = lay.hpad;
    let w = lay.w.saturating_sub(2 * lay.hpad);
    let item_x = x0 + 2;
    let full_w = w.saturating_sub(2) as usize;
    // the label column is as wide as the longest label plus tag over every item
    let col = p
        .items
        .iter()
        .map(|i| display_width(&i.label) + i.tag.as_ref().map_or(0, |t| display_width(t) + 1))
        .max()
        .unwrap_or(0)
        .min(40)
        // the bare list keeps the column Grok's own 100+ rows give it
        .max(if total_items > 20 { 30 } else { 0 });
    let sel = app.popup_sel.min(total_items.saturating_sub(1));
    // flatten items into lines, wrapping descriptions
    let mut lines: Vec<FlatLine> = Vec::new();
    let mut starts = Vec::new();
    let scroll_w = if total_items > 8 { 2 } else { 0 };
    let probe_row_w = full_w.saturating_sub(scroll_w);
    let desc_w = probe_row_w.saturating_sub(2 + col + 2);
    for (i, it) in p.items.iter().enumerate() {
        starts.push(lines.len());
        let parts = wrap_desc(&it.desc, desc_w);
        for (k, d) in parts.into_iter().enumerate() {
            lines.push(FlatLine {
                item: i,
                first: k == 0,
                desc: d,
            });
        }
    }
    let visible = lines.len().min(8);
    let scrolled = lines.len() > visible;
    let max_first = lines.len().saturating_sub(visible);
    // the file picker keeps the selection inside a window that only moves when it has to; the
    // command list keeps it centered
    let first_line = if p.kind == PopupKind::At {
        app.inp.at_scroll.min(max_first)
    } else {
        starts[sel].saturating_sub(visible / 2).min(max_first)
    };
    let top = lay.composer.y;
    if top < visible as u16 + 2 {
        return None;
    }
    Some(Geo {
        x0,
        w,
        item_x,
        full_w,
        col,
        sel,
        lines,
        visible,
        scrolled,
        first_line,
        rule_top: top - 2 - visible as u16,
    })
}

/// The item under a pointer at (`x`, `y`), if it is over a row of the open popup.
pub fn popup_hit(app: &App, lay: &Layout, x: u16, y: u16) -> Option<usize> {
    let p = popup(app)?;
    let g = geometry(app, &p, lay)?;
    if x < g.x0 || x >= g.x0 + g.w || y <= g.rule_top || y > g.rule_top + g.visible as u16 {
        return None;
    }
    let li = g.first_line + (y - g.rule_top - 1) as usize;
    g.lines.get(li).map(|l| l.item)
}

pub fn draw_popup(buf: &mut Buffer, app: &App, p: &Popup, lay: &Layout) {
    let th = &app.theme;
    let total_items = p.items.len();
    let Some(Geo {
        x0,
        w,
        item_x,
        full_w,
        col,
        sel,
        lines,
        visible,
        scrolled,
        first_line,
        rule_top,
    }) = geometry(app, p, lay)
    else {
        return;
    };
    let rule_bot = lay.composer.y - 1;
    let rule = "─".repeat(w as usize);
    put(buf, x0, rule_top, &rule, st(th.bg_light));
    put(buf, x0, rule_bot, &rule, st(th.bg_light));
    // count, right-aligned, ending two cells before the right edge of the panel
    let count = match p.kind {
        PopupKind::At if total_items >= 1000 => format!("1k+/{}", p.total),
        PopupKind::At => format!("{}/{}", total_items, p.total),
        _ => total_items.to_string(),
    };
    let cx = (x0 + w).saturating_sub(1 + display_width(&count) as u16);
    put(buf, cx, rule_top, &count, st(th.gray));
    for vy in 0..visible {
        let y = rule_top + 1 + vy as u16;
        super::fill(buf, Rect::new(x0, y, w, 1), th.bg_light);
        let li = first_line + vy;
        let Some(fl) = lines.get(li) else { continue };
        let it = &p.items[fl.item];
        let selected = fl.item == sel;
        let row_w = if scrolled {
            full_w.saturating_sub(2)
        } else {
            full_w
        } as u16;
        // the row under the pointer, when it is not the selected one
        let hovered = app.inp.popup_hover == Some(fl.item) && !selected;
        let bg = if selected {
            th.bg_visual
        } else if hovered {
            th.bg_hover
        } else {
            th.bg_light
        };
        if selected || hovered {
            super::fill(buf, Rect::new(item_x, y, row_w, 1), bg);
        }
        let clip = Rect::new(item_x, y, row_w, 1);
        if fl.first {
            let prefix = if selected { "❯ " } else { "  " };
            let mut x = put_str(
                buf,
                item_x,
                y,
                prefix,
                Style::new()
                    .fg(th.text_primary)
                    .bg(bg)
                    .add_modifier(if selected {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
                clip,
            );
            // label with matched characters picked out
            let label_style = if selected {
                Style::new()
                    .fg(th.text_primary)
                    .bg(bg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(th.text_primary).bg(bg)
            };
            let hit_style = label_style.fg(th.fuzzy_accent);
            for (ci, ch) in it.label.chars().enumerate() {
                let s = if it.hits.contains(&ci) {
                    hit_style
                } else {
                    label_style
                };
                let cw = tuikit::width::display_width(ch.encode_utf8(&mut [0u8; 4])) as u16;
                // a long path ends in `…` where the row does
                if p.kind == PopupKind::At && x + cw > clip.right() {
                    put_str(buf, x.saturating_sub(1), y, "…", label_style, clip);
                    break;
                }
                let mut b = [0u8; 4];
                x = put_str(buf, x, y, ch.encode_utf8(&mut b), s, clip);
            }
            if let Some(tag) = &it.tag {
                let tx = item_x + 2 + col as u16 - display_width(tag) as u16;
                put_str(
                    buf,
                    tx,
                    y,
                    tag,
                    Style::new().fg(th.fuzzy_accent).bg(bg),
                    clip,
                );
            }
            put_str(
                buf,
                item_x + 2 + col as u16 + 1,
                y,
                &fl.desc,
                Style::new().fg(th.gray).bg(bg),
                clip,
            );
        } else {
            put_str(
                buf,
                item_x + 2 + col as u16 + 2,
                y,
                &fl.desc,
                Style::new().fg(th.gray).bg(bg),
                clip,
            );
        }
    }
    if scrolled {
        // scrollbar: track and thumb in the last column of the panel
        let sx = (x0 + w).saturating_sub(1);
        let (pos, size) =
            super::thumb(lines.len(), visible, first_line, visible).unwrap_or((0, visible));
        for vy in 0..visible {
            let y = rule_top + 1 + vy as u16;
            if vy >= pos && vy < pos + size {
                put(buf, sx, y, "█", st(th.gray_dim).bg(th.gray_dim));
            } else {
                put(buf, sx, y, " ", Style::new().bg(th.bg_dark));
            }
        }
    }
    let _ = bold;
}
