// OWNER: shared (frame layout, line helpers; keep additive)
//! Pi draws a vertical stack of two children: a scrolling transcript (header, then every chat
//! message) and an auto-height dock (queued messages, spacer, editor or selector, autocomplete
//! popup, footer). Every component here renders to `Vec<Line>` for a given width; `draw` slices
//! the transcript to the viewport and writes the rows into the buffer. See `docs/piw-spec.md` 2.

pub mod autocomplete;
pub mod editor;
pub mod footer;
pub mod header;
pub mod hljs;
pub mod loader;
pub mod markdown_theme;
pub mod messages;
pub mod search;
pub mod selectors;
pub mod stream;
pub mod syntax;
pub mod tools;

use std::time::Duration;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use tuikit::width::{display_width, spans_width};

use crate::app::{App, Dock};
use crate::theme::{PiTheme, Tok};

pub type Lines = Vec<Line<'static>>;

/// What every component needs to render at a given width.
#[derive(Clone)]
pub struct Cx {
    pub theme: PiTheme,
    pub width: u16,
    /// `toolOutputExpanded` (`ctrl+o`): tool output, the header and summaries.
    pub expanded: bool,
    /// `hideThinkingBlock` (`ctrl+t`).
    pub hide_thinking: bool,
    /// `outputPad`: columns of left padding on message rows (setting, 0 or 1).
    pub out_pad: u16,
    pub cwd: String,
    pub home: String,
    /// Time since the app started; spinners and `Elapsed` read it, tests freeze it.
    pub clock: Duration,
    /// wizard's version, empty until it has said.
    pub version: String,
}

impl Cx {
    pub fn th(&self) -> &PiTheme {
        &self.theme
    }
}

pub fn blank() -> Line<'static> {
    Line::from("")
}

pub fn span(text: impl Into<String>, style: Style) -> Span<'static> {
    Span::styled(text.into(), style)
}

pub fn line_width(l: &Line<'_>) -> usize {
    spans_width(&l.spans)
}

/// `left` + spaces + `right`, `right` flush to column `width`. When they do not fit, `left` is cut.
pub fn spread(
    mut left: Vec<Span<'static>>,
    right: Vec<Span<'static>>,
    width: usize,
) -> Line<'static> {
    let rw = spans_width(&right);
    let lw = spans_width(&left);
    if lw + rw + 2 > width {
        left = tuikit::width::clip_spans(left, width.saturating_sub(rw + 2));
    }
    let lw = spans_width(&left);
    let gap = width.saturating_sub(lw + rw);
    left.push(Span::raw(" ".repeat(gap)));
    left.extend(right);
    Line::from(left)
}

/// Pad a row with spaces carrying `fill` up to `width`; a longer row is clipped.
pub fn pad_to(l: Line<'static>, width: usize, fill: Style) -> Line<'static> {
    let w = line_width(&l);
    let mut spans = l.spans;
    if w > width {
        spans = tuikit::width::clip_spans(spans, width);
    } else if w < width {
        spans.push(Span::styled(" ".repeat(width - w), fill));
    }
    Line::from(spans)
}

/// Pi's `Box`: `pad_x` columns each side, `pad_y` blank rows above and below, `bg` behind every
/// cell of every row up to the full width. Content rows must already fit `width - 2 * pad_x`.
pub fn boxed(content: Lines, width: u16, pad_x: u16, pad_y: u16, bg: Style) -> Lines {
    let w = width as usize;
    let mut out = Vec::with_capacity(content.len() + 2 * pad_y as usize);
    let blank_row = || Line::from(Span::styled(" ".repeat(w), bg));
    for _ in 0..pad_y {
        out.push(blank_row());
    }
    for l in content {
        let mut spans = Vec::with_capacity(l.spans.len() + 2);
        if pad_x > 0 {
            spans.push(Span::styled(" ".repeat(pad_x as usize), bg));
        }
        let cw = line_width(&l);
        for s in l.spans {
            spans.push(Span::styled(s.content, bg.patch(s.style)));
        }
        let used = pad_x as usize + cw;
        if used < w {
            spans.push(Span::styled(" ".repeat(w - used), bg));
        }
        out.push(Line::from(spans));
    }
    for _ in 0..pad_y {
        out.push(blank_row());
    }
    out
}

/// A row prefixed with `n` plain spaces.
pub fn indent(l: Line<'static>, n: u16) -> Line<'static> {
    if n == 0 {
        return l;
    }
    let mut spans = vec![Span::raw(" ".repeat(n as usize))];
    spans.extend(l.spans);
    Line::from(spans)
}

/// Wrap `text` (may hold `\n`) to `width` with one style and `Pi`'s word rules.
pub fn wrap_text(text: &str, width: u16, style: Style) -> Lines {
    let mut out = Vec::new();
    for para in tuikit::width::normalize_newlines(&tuikit::width::sanitize(text)).split('\n') {
        if para.is_empty() {
            out.push(Line::from(""));
            continue;
        }
        let rows = tuikit::width::wrap_spans(
            &[Span::styled(para.to_string(), style)],
            width.max(1) as usize,
            tuikit::width::WrapMode::Word,
        );
        for r in rows {
            out.push(Line::from(r));
        }
    }
    out
}

/// A full-width rule of `─`.
pub fn rule(width: u16, style: Style) -> Line<'static> {
    Line::from(Span::styled("─".repeat(width as usize), style))
}

/// Cut a row to `width`, ending in a dim `...` when something was dropped.
pub fn truncate_dots(l: Line<'static>, width: usize, dots: Style) -> Line<'static> {
    if line_width(&l) <= width {
        return l;
    }
    let mut spans = tuikit::width::clip_spans(l.spans, width.saturating_sub(3));
    spans.push(Span::styled("...", dots));
    Line::from(spans)
}

/// Cut `s` to at most `max` cells with no ellipsis (pi-tui's `truncateToWidth(s, max, "")`).
pub fn cut(s: &str, max: usize) -> String {
    use unicode_segmentation::UnicodeSegmentation;
    let mut out = String::new();
    let mut w = 0;
    for g in s.graphemes(true) {
        let gw = tuikit::width::grapheme_width(g);
        if w + gw > max {
            break;
        }
        out.push_str(g);
        w += gw;
    }
    out
}

pub fn home_path(p: &str, home: &str) -> String {
    if !home.is_empty() && (p == home || p.starts_with(&format!("{home}/"))) {
        format!("~{}", &p[home.len()..])
    } else {
        p.to_string()
    }
}

/// Write one row. Cells past the row's last span keep the buffer's default blank.
pub fn put_line(buf: &mut Buffer, y: u16, l: &Line<'_>, width: u16) {
    if y >= buf.area.y + buf.area.height {
        return;
    }
    buf.set_line(buf.area.x, y, l, width);
}

/// Everything the user sees, drawn into `buf` (the whole screen).
pub fn draw(buf: &mut Buffer, app: &mut App) {
    let area = buf.area;
    let (w, h) = (area.width, area.height);
    if w == 0 || h == 0 {
        return;
    }
    let cx = app.cx(w);
    let dock = dock_lines(app, &cx, h);
    let dock_h = (dock.len() as u16).min(h.saturating_sub(1));
    let view_h = (h - dock_h) as usize;
    // dock is bottom-anchored; if it is taller than the screen the top rows are dropped
    let dock_skip = dock.len().saturating_sub(dock_h as usize);

    let lines = app.transcript_chunks(&cx);
    let total = lines.len();
    app.scroll.set_extent(total, view_h);
    let mut top = app.scroll.top();
    // the search finds its matches in the rows just rendered and may scroll to the selected one
    if let Some(s) = app.search.as_mut() {
        if let Some(t) = s.refresh(lines.window(0, total), top, view_h) {
            app.scroll.pin(t);
            top = app.scroll.top();
        }
    }
    let th = &cx.theme;
    for (i, l) in lines.window(top, view_h).enumerate() {
        put_line(buf, i as u16, l, w);
    }
    if let Some(s) = &app.search {
        s.highlight(buf, top, view_h, w, th);
    }
    if app.scroll.scrolled_up() {
        draw_jump(buf, view_h, w, th);
    }
    if app.scroll.bar_visible(app.clock()) && total > view_h {
        draw_scrollbar(buf, view_h, w, top, total, th);
    }
    for (i, l) in dock.iter().skip(dock_skip).enumerate() {
        put_line(buf, view_h as u16 + i as u16, l, w);
    }
    // flashes sit over the top right, one row each, in reverse video
    for (i, (msg, _)) in app.flashes.iter().enumerate().take(h as usize) {
        let text = cut(&format!(" {msg} "), w as usize);
        let tw = display_width(&text) as u16;
        let style = Style::default().add_modifier(Modifier::REVERSED);
        // the flash replaces what is under it, whatever style that had
        for x in w - tw..w {
            if let Some(c) = buf.cell_mut((x, i as u16)) {
                c.reset();
            }
        }
        buf.set_string(w - tw, i as u16, text, style);
    }
    // an overlay is composited over everything
    if let Some(s) = &app.search {
        s.draw(buf, w);
    }
    let _ = Rect::default();
}

const JUMP: &str = " ↓ Jump to latest message · Ctrl+End ";

fn draw_jump(buf: &mut Buffer, view_h: usize, w: u16, th: &PiTheme) {
    let lw = display_width(JUMP) as u16;
    if view_h == 0 || w < lw {
        return;
    }
    let x = (w - lw) / 2;
    let y = view_h as u16 - 1;
    let style = th.fg(Tok::Text).patch(th.bg(Tok::SelectedBg));
    buf.set_string(x, y, JUMP, style);
}

fn draw_scrollbar(buf: &mut Buffer, view_h: usize, w: u16, top: usize, total: usize, th: &PiTheme) {
    let track = view_h;
    let max_top = total - view_h;
    let thumb = ((view_h * view_h) / total).clamp(1, track);
    let pos = if max_top == 0 {
        0
    } else {
        ((top as f64 / max_top as f64) * (track - thumb) as f64).round() as usize
    };
    for r in 0..track {
        let on = r >= pos && r < pos + thumb;
        let (sym, tok) = if on {
            ("┃", Tok::ScrollbarThumb)
        } else {
            ("│", Tok::ScrollbarTrack)
        };
        buf.set_string(w - 1, r as u16, sym, th.fg(tok));
    }
}

/// The dock rows, top to bottom (see the module docs). Height depends on the editor's text.
pub fn dock_lines(app: &mut App, cx: &Cx, screen_h: u16) -> Lines {
    let th = &cx.theme;
    let w = cx.width;
    let mut out: Lines = Vec::new();
    // queued messages
    if !app.queue.is_empty() {
        out.push(blank());
        let dim = th.fg(Tok::Dim);
        let row = |label: &str, text: &str| {
            let first = text.lines().next().unwrap_or("");
            let l = Line::from(vec![span(format!(" {label}: {first}"), dim)]);
            truncate_dots(l, w as usize, dim)
        };
        for q in app.queue.iter().filter(|q| !q.follow_up) {
            out.push(row("Steering", &q.text));
        }
        for q in app.queue.iter().filter(|q| q.follow_up) {
            out.push(row("Follow-up", &q.text));
        }
        let key = app.keymap.display(crate::keys::Action::Dequeue);
        out.push(Line::from(span(
            format!(" ↳ {key} to edit all queued messages"),
            dim,
        )));
    }
    // widgets above the editor: only the spacer
    out.push(blank());
    match &mut app.dock {
        Dock::Editor => {
            let max_rows = ((screen_h as usize * 3) / 10).max(5);
            let loader = app.loader(cx);
            app.editor.pad = app.settings.editor_padding_x;
            out.extend(editor::render(
                &app.editor,
                cx,
                max_rows,
                loader.as_ref(),
                app.bash_mode(),
                app.thinking_tok(),
            ));
            out.extend(app.autocomplete.render(cx));
        }
        Dock::Selector(sel) => out.extend(sel.render(cx, screen_h)),
    }
    out.extend(footer::render(app, cx));
    out
}

pub fn bold() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

/// One row as an SGR string for the exit transcript: truecolor, reset at the end. The text goes
/// to the real terminal, not through the buffer that drops controls, so every span is run through
/// `plain_text`: a model, a tool call or a file name cannot put an escape sequence in it.
pub fn ansi_line(l: &Line<'_>) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for s in &l.spans {
        let content = tuikit::width::plain_text(&s.content);
        let st = l.style.patch(s.style);
        let mut codes: Vec<String> = Vec::new();
        let m = st.add_modifier;
        for (flag, code) in [
            (Modifier::BOLD, "1"),
            (Modifier::DIM, "2"),
            (Modifier::ITALIC, "3"),
            (Modifier::UNDERLINED, "4"),
            (Modifier::REVERSED, "7"),
            (Modifier::CROSSED_OUT, "9"),
        ] {
            if m.contains(flag) {
                codes.push(code.into());
            }
        }
        let depth = tuikit::depth::current();
        let color = |c: ratatui::style::Color, fg: bool| tuikit::depth::sgr_color(c, fg, depth);
        if let Some(c) = st.fg.and_then(|c| color(c, true)) {
            codes.push(c);
        }
        if let Some(c) = st.bg.and_then(|c| color(c, false)) {
            codes.push(c);
        }
        if codes.is_empty() {
            out.push_str(&content);
        } else {
            let _ = write!(out, "\x1b[{}m{}\x1b[0m", codes.join(";"), content);
        }
    }
    out
}
