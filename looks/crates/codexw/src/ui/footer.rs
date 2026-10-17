// OWNER: bottom-pane
//! The footer row under the composer band and the shortcut overlay (spec C.4, C.5), ported from
//! Codex's `bottom_pane/footer.rs` and the footer branch of `chat_composer.rs::render`.
//!
//! One row in every mode except the overlay, which takes nine. The left side is the status line
//! (or an instructional hint that replaces it), the right side holds the mode indicator, the
//! shell label or the context text. Left and right never overlap: the left side is truncated
//! with an ellipsis, or collapses to a shorter hint, to leave one column of gap.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use super::status_indicator::truncate_with_ellipsis;
use crate::wrap::{line_width, width_of};

pub const FOOTER_INDENT_COLS: u16 = 2;
const FOOTER_CONTEXT_GAP_COLS: u16 = 1;
const MODE_CYCLE_HINT: &str = "shift+tab to cycle";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FooterMode {
    ComposerEmpty,
    ComposerHasDraft,
    ShortcutOverlay,
    EscHint,
    HistorySearch,
}

/// The collaboration mode label. Wizard has one mode beyond the default: plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CollabIndicator {
    Plan,
}

impl CollabIndicator {
    fn span(self, show_cycle_hint: bool) -> Span<'static> {
        let text = match self {
            CollabIndicator::Plan => "Plan mode",
        };
        let s = if show_cycle_hint {
            format!("{text} ({MODE_CYCLE_HINT})")
        } else {
            text.to_string()
        };
        Span::styled(s, Style::default().fg(Color::Magenta))
    }
}

#[derive(Clone, Debug)]
pub struct FooterProps {
    pub mode: FooterMode,
    pub esc_backtrack_hint: bool,
    pub is_task_running: bool,
    pub paste_burst_active: bool,
    /// Terminal reported enhanced keys, so `shift + enter` works for a newline.
    pub use_shift_enter_hint: bool,
    pub collaboration_modes_enabled: bool,
    pub status_line_enabled: bool,
    /// The configured status line value, already coloured.
    pub status_line: Option<Line<'static>>,
    pub active_agent_label: Option<String>,
    pub collab: Option<CollabIndicator>,
    /// Pre-formatted goal label (`Pursuing goal (0s)`), magenta.
    pub goal: Option<String>,
    /// `Normal` or `Insert` when Vim editing is on.
    pub vim: Option<&'static str>,
    pub shell_mode: bool,
    pub side_label: Option<String>,
    pub context_percent: Option<i64>,
    pub context_tokens: Option<i64>,
    /// `reverse-i-search:` row while Ctrl+R search is open.
    pub history_search: Option<Line<'static>>,
    /// A bold line that takes the left slot while it is set (`Save and close external editor to
    /// continue.`); the mode and context text on the right stay.
    pub hint_override: Option<String>,
}

impl Default for FooterProps {
    fn default() -> Self {
        Self {
            mode: FooterMode::ComposerEmpty,
            esc_backtrack_hint: false,
            is_task_running: false,
            paste_burst_active: false,
            use_shift_enter_hint: false,
            collaboration_modes_enabled: true,
            status_line_enabled: true,
            status_line: None,
            active_agent_label: None,
            collab: None,
            goal: None,
            vim: None,
            shell_mode: false,
            side_label: None,
            context_percent: None,
            context_tokens: None,
            history_search: None,
            hint_override: None,
        }
    }
}

// ---- left side pieces ---------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SummaryHintKind {
    None,
    Shortcuts,
    QueueMessage,
    QueueShort,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LeftSideState {
    hint: SummaryHintKind,
    show_cycle_hint: bool,
}

fn dim(s: &str) -> Span<'static> {
    Span::styled(s.to_string(), Style::default().dim())
}

fn left_side_line(collab: Option<CollabIndicator>, state: LeftSideState) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    match state.hint {
        SummaryHintKind::None => {}
        SummaryHintKind::Shortcuts => {
            spans.push(dim("?"));
            spans.push(dim(" for shortcuts"));
        }
        SummaryHintKind::QueueMessage => {
            spans.push(dim("tab"));
            spans.push(dim(" to queue message"));
        }
        SummaryHintKind::QueueShort => {
            spans.push(dim("tab"));
            spans.push(dim(" to queue"));
        }
    }
    if let Some(c) = collab {
        if state.hint != SummaryHintKind::None {
            spans.push(dim(" · "));
        }
        spans.push(c.span(state.show_cycle_hint));
    }
    Line::from(spans)
}

pub enum SummaryLeft {
    Default,
    Custom(Line<'static>),
    None,
}

fn right_aligned_x(area: Rect, content_width: u16) -> Option<u16> {
    if area.is_empty() {
        return None;
    }
    let max_width = area.width.saturating_sub(FOOTER_INDENT_COLS);
    if content_width == 0 || max_width == 0 {
        return None;
    }
    if content_width >= max_width {
        return Some(area.x + FOOTER_INDENT_COLS);
    }
    Some(area.x + area.width - content_width - FOOTER_INDENT_COLS)
}

pub fn max_left_width_for_right(area: Rect, right_width: u16) -> Option<u16> {
    let context_x = right_aligned_x(area, right_width)?;
    let left_start = area.x + FOOTER_INDENT_COLS;
    if context_x <= left_start + FOOTER_CONTEXT_GAP_COLS {
        return Some(0);
    }
    Some(context_x - (left_start + FOOTER_CONTEXT_GAP_COLS))
}

pub fn can_show_left_with_context(area: Rect, left_width: u16, context_width: u16) -> bool {
    let Some(context_x) = right_aligned_x(area, context_width) else {
        return true;
    };
    if left_width == 0 {
        return true;
    }
    let left_extent = FOOTER_INDENT_COLS + left_width + FOOTER_CONTEXT_GAP_COLS;
    left_extent <= context_x.saturating_sub(area.x)
}

fn left_fits(area: Rect, left_width: u16) -> bool {
    left_width <= area.width.saturating_sub(FOOTER_INDENT_COLS)
}

/// Single-row collapse when the status line is off (spec C.4.4): the first variant that fits wins.
pub fn single_line_footer_layout(
    area: Rect,
    context_width: u16,
    collab: Option<CollabIndicator>,
    show_cycle_hint: bool,
    show_shortcuts_hint: bool,
    show_queue_hint: bool,
) -> (SummaryLeft, bool) {
    let hint_kind = if show_queue_hint {
        SummaryHintKind::QueueMessage
    } else if show_shortcuts_hint {
        SummaryHintKind::Shortcuts
    } else {
        SummaryHintKind::None
    };
    let default_state = LeftSideState {
        hint: hint_kind,
        show_cycle_hint,
    };
    let default_line = left_side_line(collab, default_state);
    let default_width = line_width(&default_line) as u16;
    if default_width > 0 && can_show_left_with_context(area, default_width, context_width) {
        return (SummaryLeft::Default, true);
    }
    let state_line = |s: LeftSideState| left_side_line(collab, s);
    let state_width = |s: LeftSideState| line_width(&state_line(s)) as u16;
    let context_requires_cycle_hint = show_cycle_hint && !show_queue_hint;

    if show_queue_hint {
        let queue_states = [
            default_state,
            LeftSideState {
                hint: SummaryHintKind::QueueMessage,
                show_cycle_hint: false,
            },
            LeftSideState {
                hint: SummaryHintKind::QueueShort,
                show_cycle_hint: false,
            },
        ];
        let mut prev: Option<LeftSideState> = None;
        for s in queue_states {
            if prev == Some(s) {
                continue;
            }
            prev = Some(s);
            let w = state_width(s);
            if w > 0 && can_show_left_with_context(area, w, context_width) {
                if s == default_state {
                    return (SummaryLeft::Default, true);
                }
                return (SummaryLeft::Custom(state_line(s)), true);
            }
        }
        let mut prev: Option<LeftSideState> = None;
        for s in queue_states {
            if prev == Some(s) {
                continue;
            }
            prev = Some(s);
            let w = state_width(s);
            if w > 0 && left_fits(area, w) {
                if s == default_state {
                    return (SummaryLeft::Default, false);
                }
                return (SummaryLeft::Custom(state_line(s)), false);
            }
        }
    } else if collab.is_some() {
        if show_cycle_hint {
            let cycle_state = LeftSideState {
                hint: SummaryHintKind::None,
                show_cycle_hint: true,
            };
            let cw = state_width(cycle_state);
            if cw > 0 && can_show_left_with_context(area, cw, context_width) {
                return (SummaryLeft::Custom(state_line(cycle_state)), true);
            }
            if cw > 0 && left_fits(area, cw) {
                return (SummaryLeft::Custom(state_line(cycle_state)), false);
            }
        }
        let mode_only = LeftSideState {
            hint: SummaryHintKind::None,
            show_cycle_hint: false,
        };
        let mw = state_width(mode_only);
        if !context_requires_cycle_hint
            && mw > 0
            && can_show_left_with_context(area, mw, context_width)
        {
            return (SummaryLeft::Custom(state_line(mode_only)), true);
        }
        if mw > 0 && left_fits(area, mw) {
            return (SummaryLeft::Custom(state_line(mode_only)), false);
        }
    }

    if collab.is_some() {
        let mode_only = LeftSideState {
            hint: SummaryHintKind::None,
            show_cycle_hint: false,
        };
        let mw = line_width(&left_side_line(collab, mode_only)) as u16;
        if !context_requires_cycle_hint && can_show_left_with_context(area, mw, context_width) {
            return (SummaryLeft::Custom(left_side_line(collab, mode_only)), true);
        }
        if left_fits(area, mw) {
            return (
                SummaryLeft::Custom(left_side_line(collab, mode_only)),
                false,
            );
        }
    }
    (SummaryLeft::None, true)
}

// ---- right side ---------------------------------------------------------------------------

fn vim_span(label: &str) -> Span<'static> {
    match label {
        "Normal" => Span::styled("Vim: Normal", Style::default().fg(Color::Magenta)),
        _ => Span::styled("Vim: Insert", Style::default().fg(Color::Green)),
    }
}

/// Compact token count: `999`, `12.3K`, `4.5M`.
pub fn format_tokens_compact(n: i64) -> String {
    let n = n.max(0) as f64;
    if n >= 1_000_000.0 {
        trim_float(n / 1_000_000.0, "M")
    } else if n >= 1_000.0 {
        trim_float(n / 1_000.0, "K")
    } else {
        format!("{}", n as i64)
    }
}

fn trim_float(v: f64, suffix: &str) -> String {
    if v >= 100.0 {
        format!("{}{suffix}", v.round() as i64)
    } else {
        let s = format!("{v:.1}");
        let s = s.strip_suffix(".0").unwrap_or(&s);
        format!("{s}{suffix}")
    }
}

/// `N% context left`, `12K used`, or nothing when wizard reports neither.
fn context_window_line(percent: Option<i64>, tokens: Option<i64>) -> Line<'static> {
    if let Some(p) = percent {
        return Line::from(dim(&format!("{}% context left", p.clamp(0, 100))));
    }
    if let Some(t) = tokens {
        return Line::from(dim(&format!("{} used", format_tokens_compact(t))));
    }
    Line::default()
}

fn goal_line(label: &str) -> Line<'static> {
    Line::from(Span::styled(
        label.to_string(),
        Style::default().fg(Color::Magenta),
    ))
}

/// The right slot while the status line is on: Vim, then the mode or goal label.
fn mode_indicator_line(props: &FooterProps, show_cycle_hint: bool) -> Option<Line<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    if let Some(v) = props.vim {
        spans.push(vim_span(v));
    }
    let primary = props
        .collab
        .map(|c| Line::from(c.span(show_cycle_hint)))
        .or_else(|| props.goal.as_deref().map(goal_line));
    if let Some(p) = primary {
        if !spans.is_empty() {
            spans.push(dim(" | "));
        }
        spans.extend(p.spans);
    }
    (!spans.is_empty()).then(|| Line::from(spans))
}

fn side_conversation_line(label: &str) -> Line<'static> {
    let magenta = Style::default().fg(Color::Magenta);
    if let Some(rest) = label.strip_prefix("Side ") {
        Line::from(vec![
            Span::styled("Side", magenta.bold()),
            Span::styled(format!(" {rest}"), magenta),
        ])
    } else {
        Line::from(Span::styled(label.to_string(), magenta))
    }
}

fn right_footer_line_with_context(props: &FooterProps) -> Line<'static> {
    let mut line = context_window_line(props.context_percent, props.context_tokens);
    if let Some(v) = props.vim {
        if !line.spans.is_empty() {
            line.spans.push(dim(" | "));
        }
        line.spans.push(vim_span(v));
    }
    line
}

// ---- modes ---------------------------------------------------------------------------------

fn esc_hint_line(backtrack: bool) -> Line<'static> {
    if backtrack {
        Line::from(vec![dim("esc"), dim(" again to edit previous message")])
    } else {
        Line::from(vec![
            dim("esc"),
            dim(" "),
            dim("esc"),
            dim(" to edit previous message"),
        ])
    }
}

/// The contextual row shown when no instructional hint is active.
fn shows_passive_footer_line(props: &FooterProps) -> bool {
    match props.mode {
        FooterMode::ComposerEmpty => true,
        FooterMode::ComposerHasDraft => !props.is_task_running,
        _ => false,
    }
}

fn uses_passive_footer_status_layout(props: &FooterProps) -> bool {
    props.hint_override.is_some() || (props.status_line_enabled && shows_passive_footer_line(props))
}

/// Codex's `footer_hint_items_line` for one item with no label: a space, the key text in bold, a
/// space.
fn hint_override_line(text: &str) -> Line<'static> {
    Line::from(vec![
        Span::raw(" "),
        Span::styled(text.to_string(), Style::default().bold()),
        Span::raw(" "),
    ])
}

fn passive_footer_status_line(props: &FooterProps) -> Option<Line<'static>> {
    if let Some(t) = &props.hint_override {
        return Some(hint_override_line(t));
    }
    if !shows_passive_footer_line(props) {
        return None;
    }
    let mut line = if props.status_line_enabled {
        props.status_line.clone()
    } else {
        None
    };
    if let Some(label) = &props.active_agent_label {
        match line.as_mut() {
            Some(l) => {
                l.spans.push(dim(" · "));
                l.spans.push(dim(label));
            }
            None => line = Some(Line::from(dim(label))),
        }
    }
    line
}

fn show_shortcuts_hint(props: &FooterProps) -> bool {
    match props.mode {
        FooterMode::ComposerEmpty => !props.paste_burst_active,
        _ => false,
    }
}

fn show_queue_hint(props: &FooterProps) -> bool {
    matches!(props.mode, FooterMode::ComposerHasDraft) && props.is_task_running
}

/// Rows the footer takes: nine for the overlay, one otherwise.
pub fn footer_height(props: &FooterProps) -> u16 {
    if props.mode == FooterMode::ShortcutOverlay {
        shortcut_overlay_lines(props).len() as u16
    } else {
        1
    }
}

// ---- rendering -----------------------------------------------------------------------------

fn render_footer_line(area: Rect, buf: &mut Buffer, line: &Line<'static>, row: u16) {
    if area.is_empty() || row >= area.height {
        return;
    }
    let inner = Rect::new(
        area.x + FOOTER_INDENT_COLS.min(area.width),
        area.y + row,
        area.width.saturating_sub(FOOTER_INDENT_COLS),
        1,
    );
    if inner.is_empty() {
        return;
    }
    tuikit::paint::put_line(buf, inner.x, inner.y, line, inner);
}

fn render_context_right(area: Rect, buf: &mut Buffer, line: &Line<'static>) {
    if area.is_empty() {
        return;
    }
    let w = line_width(line) as u16;
    let Some(x) = right_aligned_x(area, w) else {
        return;
    };
    let y = area.y + area.height.saturating_sub(1);
    let clip = Rect::new(x, y, area.right().saturating_sub(x), 1);
    tuikit::paint::put_line(buf, x, y, line, clip);
}

/// Draw the footer into `area` (its full height: 1 row, or 9 for the overlay).
pub fn render_footer(area: Rect, buf: &mut Buffer, props: &FooterProps) {
    if area.is_empty() {
        return;
    }
    if let Some(line) = &props.history_search {
        render_footer_line(area, buf, line, 0);
        return;
    }
    match props.mode {
        FooterMode::ShortcutOverlay => {
            for (i, l) in shortcut_overlay_lines(props).iter().enumerate() {
                render_footer_line(area, buf, l, i as u16);
            }
            return;
        }
        FooterMode::EscHint => {
            render_footer_line(area, buf, &esc_hint_line(props.esc_backtrack_hint), 0);
            return;
        }
        FooterMode::HistorySearch => {
            render_footer_line(area, buf, &Line::from(dim("reverse-i-search: ")), 0);
            return;
        }
        _ => {}
    }

    let show_cycle_hint = !props.is_task_running && props.collab.is_some();
    let shortcuts = show_shortcuts_hint(props);
    let queue = show_queue_hint(props);
    let available = area.width.saturating_sub(FOOTER_INDENT_COLS) as usize;
    let status_active = uses_passive_footer_status_layout(props);
    let combined = if status_active {
        passive_footer_status_line(props)
    } else {
        None
    };
    let mut truncated = combined
        .as_ref()
        .map(|l| truncate_with_ellipsis(l.clone(), available));
    let left_collab = if status_active { None } else { props.collab };
    let mut left_width = if status_active {
        truncated.as_ref().map_or(0, |l| line_width(l) as u16)
    } else {
        let state = LeftSideState {
            hint: if queue {
                SummaryHintKind::QueueMessage
            } else if shortcuts {
                SummaryHintKind::Shortcuts
            } else {
                SummaryHintKind::None
            },
            show_cycle_hint,
        };
        line_width(&left_side_line(left_collab, state)) as u16
    };
    let right_line: Option<Line<'static>> = if let Some(l) = &props.side_label {
        Some(side_conversation_line(l))
    } else if props.shell_mode {
        Some(Line::from(Span::styled(
            "Shell mode",
            Style::default().fg(Color::LightRed),
        )))
    } else if status_active {
        let full = mode_indicator_line(props, show_cycle_hint);
        let compact = mode_indicator_line(props, false);
        let full_w = full.as_ref().map_or(0, |l| line_width(l) as u16);
        if can_show_left_with_context(area, left_width, full_w) {
            full
        } else {
            compact
        }
    } else {
        Some(right_footer_line_with_context(props))
    };
    let right_width = right_line.as_ref().map_or(0, |l| line_width(l) as u16);
    if status_active {
        if let Some(max_left) = max_left_width_for_right(area, right_width) {
            if left_width > max_left {
                if let Some(l) = combined
                    .as_ref()
                    .map(|l| truncate_with_ellipsis(l.clone(), max_left as usize))
                {
                    left_width = line_width(&l) as u16;
                    truncated = Some(l);
                }
            }
        }
    }
    let can_show_left_and_context = can_show_left_with_context(area, left_width, right_width);
    let single_line_layout = if status_active {
        None
    } else {
        Some(single_line_footer_layout(
            area,
            right_width,
            left_collab,
            show_cycle_hint,
            shortcuts,
            queue,
        ))
    };
    let show_right = single_line_layout
        .as_ref()
        .map_or(can_show_left_and_context, |(_, ctx)| *ctx);

    let state = LeftSideState {
        hint: if queue {
            SummaryHintKind::QueueMessage
        } else if shortcuts {
            SummaryHintKind::Shortcuts
        } else {
            SummaryHintKind::None
        },
        show_cycle_hint,
    };
    match single_line_layout {
        Some((SummaryLeft::Default, _)) => {
            render_footer_line(area, buf, &left_side_line(left_collab, state), 0);
        }
        Some((SummaryLeft::Custom(l), _)) => render_footer_line(area, buf, &l, 0),
        Some((SummaryLeft::None, _)) => {}
        None => {
            if let Some(l) = truncated {
                render_footer_line(area, buf, &l, 0);
            }
        }
    }
    if show_right {
        if let Some(l) = &right_line {
            render_context_right(area, buf, l);
        }
    }
}

// ---- shortcut overlay -------------------------------------------------------------------------

/// Nine rows (spec C.5): two columns of entries, a blank row, and the `/keymap` pointer.
pub fn shortcut_overlay_lines(props: &FooterProps) -> Vec<Line<'static>> {
    let newline = if props.use_shift_enter_hint {
        "shift + enter for newline"
    } else {
        "ctrl + j for newline"
    };
    let queue = if props.is_task_running {
        "tab to queue message"
    } else {
        "tab to submit message"
    };
    let edit_previous = if props.esc_backtrack_hint {
        "esc again to edit previous message"
    } else {
        "esc esc to edit previous message"
    };
    let quit = if props.is_task_running {
        "ctrl + c to interrupt"
    } else {
        "ctrl + c to exit"
    };
    let mut entries: Vec<&str> = vec![
        "/ for commands",
        "! for shell commands",
        newline,
        queue,
        "@ for file paths",
        "ctrl + v to paste images",
        "ctrl + g to edit in external editor",
        edit_previous,
        "ctrl + r search history",
        quit,
        "alt + , reasoning down",
        "alt + . reasoning up",
    ];
    if props.collaboration_modes_enabled {
        entries.push("shift + tab to change mode");
    }
    entries.push("ctrl + t to view transcript");
    let mut lines = build_columns(&entries);
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::raw("customize shortcuts with "),
        Span::styled("/keymap", Style::default().fg(Color::Cyan)),
    ]));
    lines
}

fn build_columns(entries: &[&str]) -> Vec<Line<'static>> {
    const COLUMNS: usize = 2;
    const COLUMN_PADDING: usize = 4;
    const COLUMN_GAP: usize = 4;
    if entries.is_empty() {
        return Vec::new();
    }
    let rows = entries.len().div_ceil(COLUMNS);
    let mut padded: Vec<&str> = entries.to_vec();
    padded.resize(rows * COLUMNS, "");
    let mut widths = [0usize; COLUMNS];
    for (i, e) in padded.iter().enumerate() {
        widths[i % COLUMNS] = widths[i % COLUMNS].max(width_of(e));
    }
    for w in &mut widths {
        *w += COLUMN_PADDING;
    }
    padded
        .chunks(COLUMNS)
        .map(|chunk| {
            let mut s = String::new();
            for (col, e) in chunk.iter().enumerate() {
                s.push_str(e);
                if col < COLUMNS - 1 {
                    let pad = widths[col].saturating_sub(width_of(e)) + COLUMN_GAP;
                    s.push_str(&" ".repeat(pad));
                }
            }
            Line::from(Span::styled(s, Style::default().dim()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(area_w: u16, props: &FooterProps) -> String {
        let h = footer_height(props);
        let area = Rect::new(0, 0, area_w, h);
        let mut buf = Buffer::empty(area);
        render_footer(area, &mut buf, props);
        (0..h)
            .map(|y| {
                let s: String = (0..area_w)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect();
                s
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn off(mode: FooterMode) -> FooterProps {
        FooterProps {
            mode,
            status_line_enabled: false,
            context_percent: Some(100),
            ..Default::default()
        }
    }

    // The collapse rows below are the last line of Codex's `footer_collapse_*` snapshots.

    #[test]
    fn idle_shortcuts_and_context() {
        let p = off(FooterMode::ComposerEmpty);
        assert_eq!(
            row(60, &p),
            "  ? for shortcuts                        100% context left  "
        );
        assert_eq!(row(44, &p), "  ? for shortcuts        100% context left  ");
        assert_eq!(row(26, &p), "       100% context left  ");
    }

    #[test]
    fn plan_mode_collapse() {
        let mut p = off(FooterMode::ComposerEmpty);
        p.collab = Some(CollabIndicator::Plan);
        assert_eq!(
            row(60, &p),
            "  Plan mode (shift+tab to cycle)         100% context left  "
        );
        assert_eq!(row(44, &p), "  Plan mode (shift+tab to cycle)            ");
        assert_eq!(row(26, &p), "  Plan mode               ");
        assert_eq!(
            row(120, &p),
            format!(
                "  ? for shortcuts · Plan mode (shift+tab to cycle){}100% context left  ",
                " ".repeat(51)
            )
        );
    }

    #[test]
    fn queue_hint_collapse() {
        let mut p = off(FooterMode::ComposerHasDraft);
        p.is_task_running = true;
        p.context_percent = Some(98);
        assert_eq!(
            row(50, &p),
            "  tab to queue message          98% context left  "
        );
        assert_eq!(row(40, &p), "  tab to queue        98% context left  ");
        assert_eq!(row(30, &p), "  tab to queue message        ");
        assert_eq!(row(20, &p), "  tab to queue      ");
        p.collab = Some(CollabIndicator::Plan);
        assert_eq!(
            row(50, &p),
            "  tab to queue · Plan mode      98% context left  "
        );
        assert_eq!(row(40, &p), "  tab to queue message · Plan mode      ");
        assert_eq!(row(30, &p), "  tab to queue · Plan mode    ");
        assert_eq!(row(20, &p), "  Plan mode         ");
    }

    #[test]
    fn status_line_shell_label_and_truncation() {
        let mut p = FooterProps {
            status_line: Some(Line::from("gpt-5.5 default · ~/proj")),
            shell_mode: true,
            ..Default::default()
        };
        let r = row(40, &p);
        assert_eq!(r, "  gpt-5.5 default · ~/proj  Shell mode  ");
        p.status_line = Some(Line::from(
            "gpt-5.5 default · ~/proj/with/a/very/long/path/here",
        ));
        let r = row(40, &p);
        assert!(r.starts_with("  gpt-5.5 default"));
        assert!(r.contains("…"));
        assert!(r.trim_end().ends_with("Shell mode"));
    }

    #[test]
    fn right_label_ends_at_col_118_of_120() {
        let p = FooterProps {
            shell_mode: true,
            status_line: Some(Line::from("x")),
            ..Default::default()
        };
        let r = row(120, &p);
        assert_eq!(r.trim_end().len(), 118);
    }

    #[test]
    fn esc_hint_replaces_the_status_line() {
        let mut p = FooterProps {
            mode: FooterMode::EscHint,
            esc_backtrack_hint: true,
            status_line: Some(Line::from("gpt-5.5 default · ~/proj")),
            ..Default::default()
        };
        assert_eq!(
            row(80, &p).trim_end(),
            "  esc again to edit previous message"
        );
        p.esc_backtrack_hint = false;
        assert_eq!(row(80, &p).trim_end(), "  esc esc to edit previous message");
    }

    #[test]
    fn overlay_matches_the_capture() {
        let p = FooterProps {
            mode: FooterMode::ShortcutOverlay,
            ..Default::default()
        };
        let want = [
            "  / for commands                             ! for shell commands",
            "  ctrl + j for newline                       tab to submit message",
            "  @ for file paths                           ctrl + v to paste images",
            "  ctrl + g to edit in external editor        esc esc to edit previous message",
            "  ctrl + r search history                    ctrl + c to exit",
            "  alt + , reasoning down                     alt + . reasoning up",
            "  shift + tab to change mode                 ctrl + t to view transcript",
            "",
            "  customize shortcuts with /keymap",
        ];
        let got = row(120, &p);
        let got: Vec<&str> = got.lines().map(|l| l.trim_end()).collect();
        assert_eq!(got, want);
    }

    #[test]
    fn overlay_clips_instead_of_reflowing() {
        let p = FooterProps {
            mode: FooterMode::ShortcutOverlay,
            ..Default::default()
        };
        let got = row(60, &p);
        let first = got.lines().next().unwrap();
        assert_eq!(
            first,
            "  / for commands                             ! for shell com"
        );
    }

    #[test]
    fn running_overlay_swaps_two_entries() {
        let p = FooterProps {
            mode: FooterMode::ShortcutOverlay,
            is_task_running: true,
            ..Default::default()
        };
        let got = row(120, &p);
        assert!(got.contains("tab to queue message"));
        assert!(got.contains("ctrl + c to interrupt"));
    }

    #[test]
    fn context_text_forms() {
        assert_eq!(format_tokens_compact(123_000), "123K");
        assert_eq!(format_tokens_compact(1_500), "1.5K");
        assert_eq!(format_tokens_compact(999), "999");
        let mut p = off(FooterMode::ComposerEmpty);
        p.context_percent = None;
        p.context_tokens = Some(123_000);
        assert!(row(80, &p).trim_end().ends_with("123K used"));
        p.context_tokens = None;
        assert_eq!(row(80, &p).trim_end(), "  ? for shortcuts");
    }

    #[test]
    fn vim_label_joins_context_and_mode_colours() {
        let mut p = FooterProps {
            vim: Some("Normal"),
            status_line: Some(Line::from("m")),
            ..Default::default()
        };
        let area = Rect::new(0, 0, 40, 1);
        let mut buf = Buffer::empty(area);
        render_footer(area, &mut buf, &p);
        assert_eq!(buf[(28, 0)].fg, Color::Magenta);
        p.vim = Some("Insert");
        let mut buf = Buffer::empty(area);
        render_footer(area, &mut buf, &p);
        assert_eq!(buf[(28, 0)].fg, Color::Green);
    }
}
