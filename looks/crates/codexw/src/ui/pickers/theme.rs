//! The `/theme` picker (spec C.7.5): every bundled syntax theme and any `.tmTheme` under
//! `~/.config/codexw/themes`, a live preview of a small Rust diff beside the list (stacked under
//! it when the terminal is too narrow), the old theme back on Esc, the saved name in
//! `~/.config/codexw/config.toml` on Enter. Ported from Codex's `theme_picker.rs`.

use std::path::Path;
use std::sync::Arc;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use super::selection::{
    ItemAction, SelectionItem, SelectionParams, SideContent, SideParams, side_by_side_layout_widths,
};
use crate::highlight;
use crate::ui::AppAction;
use crate::ui::diff_render::{PreviewKind, line_number_width, preview_diff_line};
use crate::width::display_width;

struct Row {
    line_no: usize,
    kind: PreviewKind,
    code: &'static str,
}

const fn row(line_no: usize, kind: PreviewKind, code: &'static str) -> Row {
    Row {
        line_no,
        kind,
        code,
    }
}

use PreviewKind::{Added, Context, Removed};

/// Four rows for the stacked layout: one removed, one added line always visible.
const NARROW_ROWS: [Row; 4] = [
    row(12, Context, "fn greet(name: &str) -> String {"),
    row(13, Removed, "    format!(\"Hello, {}!\", name)"),
    row(13, Added, "    format!(\"Hello, {name}!\")"),
    row(14, Context, "}"),
];

/// The side-by-side sample: context, additions and removals mixed.
const WIDE_ROWS: [Row; 8] = [
    row(31, Context, "fn summarize(users: &[User]) -> String {"),
    row(
        32,
        Removed,
        "    let active = users.iter().filter(|u| u.is_active).count();",
    ),
    row(
        32,
        Added,
        "    let active = users.iter().filter(|u| u.is_active()).count();",
    ),
    row(
        33,
        Context,
        "    let names: Vec<&str> = users.iter().map(User::name).take(3).collect();",
    ),
    row(
        34,
        Removed,
        "    format!(\"{} active: {}\", active, names.join(\", \"))",
    ),
    row(
        34,
        Added,
        "    format!(\"{active} active users: {}\", names.join(\", \"))",
    ),
    row(35, Added, "        .trim()"),
    row(36, Context, "}"),
];

/// Smallest side panel the preview is drawn beside the list.
const WIDE_MIN_WIDTH: u16 = 44;
const WIDE_LEFT_INSET: u16 = 2;
const FRAME_PADDING: u16 = 1;
const FALLBACK_SUBTITLE: &str = "Move up/down to live preview themes";

fn centered_offset(available: u16, content: u16, min_frame: u16) -> u16 {
    let free = available.saturating_sub(content);
    let frame = if free >= min_frame.saturating_mul(2) {
        min_frame
    } else {
        0
    };
    frame + free.saturating_sub(frame.saturating_mul(2)) / 2
}

struct Preview {
    rows: &'static [Row],
    center: bool,
    left_inset: u16,
}

impl SideContent for Preview {
    fn desired_height(&self, _width: u16) -> u16 {
        if self.center {
            u16::MAX
        } else {
            self.rows.len() as u16
        }
    }

    fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.is_empty() || self.rows.is_empty() {
            return;
        }
        let code = self
            .rows
            .iter()
            .map(|r| r.code)
            .collect::<Vec<_>>()
            .join("\n");
        let spans = highlight::highlight_spans(&code, highlight::find_syntax("rust"));
        let ln_width = line_number_width(self.rows.iter().map(|r| r.line_no).max().unwrap_or(1));
        let content_h = (self.rows.len() as u16).min(area.height);
        let left = self.left_inset.min(area.width.saturating_sub(1));
        let top = if self.center {
            centered_offset(area.height, content_h, FRAME_PADDING)
        } else {
            0
        };
        let width = area.width.saturating_sub(left);
        for (y, (i, r)) in (area.y + top..).zip(self.rows.iter().enumerate()) {
            if y >= area.bottom() {
                break;
            }
            let line = preview_diff_line(
                r.line_no,
                r.kind,
                r.code,
                width as usize,
                ln_width,
                spans.get(i).map(Vec::as_slice),
            );
            line.render(Rect::new(area.x + left, y, width, 1), buf);
        }
    }
}

/// `Custom .tmTheme files can be added to the ~/.config/codexw/themes directory.` when it fits
/// the list column, else the short line.
fn subtitle(dir_display: Option<&str>, terminal_width: u16) -> String {
    let content = terminal_width.saturating_sub(4);
    let available = side_by_side_layout_widths(content, WIDE_MIN_WIDTH)
        .map_or(content, |(list, _)| list) as usize;
    if let Some(d) = dir_display.filter(|d| d.starts_with('~')) {
        let s = format!("Custom .tmTheme files can be added to the {d}/themes directory.");
        if display_width(&s) <= available {
            return s;
        }
    }
    FALLBACK_SUBTITLE.to_string()
}

/// The picker's parameters. `current` is the saved theme name; the highlight starts on it, or on
/// the adaptive default when it is not one of the entries.
pub fn theme_picker_params(
    current: Option<&str>,
    config_dir: Option<&Path>,
    dir_display: Option<&str>,
    terminal_width: u16,
) -> SelectionParams {
    let previous = highlight::explicit_syntax_theme();
    let entries = highlight::list_available_themes(config_dir);
    let effective = current
        .filter(|n| entries.iter().any(|e| e.name == *n))
        .map(str::to_string)
        .unwrap_or_else(|| highlight::adaptive_default_theme_name().to_string());
    let mut initial = None;
    let items: Vec<SelectionItem> = entries
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let display = if e.is_custom {
                format!("{} (custom)", e.name)
            } else {
                e.name.clone()
            };
            let mut it = SelectionItem::new(
                display,
                ItemAction::Close(AppAction::SyntaxThemeSelected(e.name.clone())),
            );
            it.is_current = e.name == effective;
            if it.is_current {
                initial = Some(i);
            }
            it.search_value = Some(e.name.clone());
            it
        })
        .collect();
    let names: Vec<String> = entries.iter().map(|e| e.name.clone()).collect();
    let dir = config_dir.map(Path::to_path_buf);
    let on_selection_changed = Arc::new(move |i: usize| {
        if let Some(t) = names
            .get(i)
            .and_then(|n| highlight::resolve_theme_by_name(n, dir.as_deref()))
        {
            highlight::set_syntax_theme(t);
        }
    });
    let on_dismiss = Arc::new(move || highlight::restore_syntax_theme(previous.clone()));
    SelectionParams {
        title: Some("Select Syntax Theme".into()),
        subtitle: Some(subtitle(dir_display, terminal_width)),
        items,
        is_searchable: true,
        search_placeholder: Some("Type to filter themes...".into()),
        initial_selected_idx: initial,
        side: Some(SideParams {
            wide: Arc::new(Preview {
                rows: &WIDE_ROWS,
                center: true,
                left_inset: WIDE_LEFT_INSET,
            }),
            stacked: Some(Arc::new(Preview {
                rows: &NARROW_ROWS,
                center: false,
                left_inset: 0,
            })),
            min_width: WIDE_MIN_WIDTH,
            preserve_bg: true,
        }),
        on_selection_changed: Some(on_selection_changed),
        on_dismiss: Some(on_dismiss),
        ..Default::default()
    }
}
