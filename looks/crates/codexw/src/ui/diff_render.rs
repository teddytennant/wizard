// OWNER: history-cells
//! File-change cells (spec B.6): the `• Edited path (+N -M)` summary with numbered, tinted diff
//! rows under it, ported from Codex's `diff_render.rs`.
//!
//! Wizard sends no hunks. An `edit_file` call carries the replaced snippet and its replacement,
//! so an update is diffed here from those two strings, with line numbers counted from the top of
//! the snippet. A `write_file` call is an add.

use std::path::Path;

use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use similar::{ChangeTag, TextDiff};
use unicode_width::UnicodeWidthChar;

use super::HistoryCell;
use crate::highlight::{MAX_HIGHLIGHT_BYTES, MAX_HIGHLIGHT_LINES, find_syntax, highlight_spans};
use crate::style::{ColorLevel, Palette, palette};

const TAB_REPLACEMENT: &str = "    ";
const TAB_WIDTH: usize = TAB_REPLACEMENT.len();
const CONTEXT_LINES: usize = 3;
const TOOL_CALL_MAX_LINES: usize = 5;

const DARK_TC_ADD_LINE_BG_RGB: (u8, u8, u8) = (33, 58, 43);
const DARK_TC_DEL_LINE_BG_RGB: (u8, u8, u8) = (74, 34, 29);
const LIGHT_TC_ADD_LINE_BG_RGB: (u8, u8, u8) = (218, 251, 225);
const LIGHT_TC_DEL_LINE_BG_RGB: (u8, u8, u8) = (255, 235, 233);
const LIGHT_TC_GUTTER_FG_RGB: (u8, u8, u8) = (31, 35, 40);

const DARK_256_ADD_LINE_BG_IDX: u8 = 22;
const DARK_256_DEL_LINE_BG_IDX: u8 = 52;
const LIGHT_256_ADD_LINE_BG_IDX: u8 = 194;
const LIGHT_256_DEL_LINE_BG_IDX: u8 = 224;
const LIGHT_256_GUTTER_FG_IDX: u8 = 236;

/// One file's change, as the cell shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    Add {
        content: String,
    },
    Delete {
        content: String,
    },
    /// `old` and `new` are the replaced snippet and its replacement, not whole files.
    Update {
        old: String,
        new: String,
        move_to: Option<String>,
    },
}

/// The `• Edited ...` cell. Rows are sorted by path.
#[derive(Clone, Debug)]
pub struct PatchCell {
    files: Vec<(String, Change)>,
}

impl PatchCell {
    /// `files` carry display paths (relative to the working directory where possible).
    pub fn new(mut files: Vec<(String, Change)>) -> Self {
        files.sort_by(|a, b| Path::new(&a.0).cmp(Path::new(&b.0)));
        PatchCell { files }
    }
}

impl HistoryCell for PatchCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        render_changes_block(&self.files, width as usize)
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        // Codex renders the summary at width 10 000 so nothing wraps.
        crate::ui::plain_lines(render_changes_block(&self.files, 10_000))
    }
}

// ---- colours ---------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DiffTheme {
    Dark,
    Light,
}

/// Palette depth the diff targets: 16 colour terminals get no tint at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DiffColorLevel {
    TrueColor,
    Ansi256,
    Ansi16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RichLevel {
    TrueColor,
    Ansi256,
}

impl DiffColorLevel {
    fn rich(self) -> Option<RichLevel> {
        match self {
            DiffColorLevel::TrueColor => Some(RichLevel::TrueColor),
            DiffColorLevel::Ansi256 => Some(RichLevel::Ansi256),
            DiffColorLevel::Ansi16 => None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct DiffBackgrounds {
    add: Option<Color>,
    del: Option<Color>,
}

#[derive(Clone, Copy, Debug)]
struct StyleContext {
    theme: DiffTheme,
    level: DiffColorLevel,
    bgs: DiffBackgrounds,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LineKind {
    Insert,
    Delete,
    Context,
}

fn rgb(c: (u8, u8, u8)) -> Color {
    Color::Rgb(c.0, c.1, c.2)
}

fn context_for(p: &Palette) -> StyleContext {
    let theme = if p.light_bg() {
        DiffTheme::Light
    } else {
        DiffTheme::Dark
    };
    let level = match p.level {
        ColorLevel::TrueColor => DiffColorLevel::TrueColor,
        ColorLevel::Ansi256 => DiffColorLevel::Ansi256,
        ColorLevel::Ansi16 => DiffColorLevel::Ansi16,
    };
    let bgs = match level.rich() {
        Some(l) => {
            // The syntax theme may paint inserted and deleted lines itself (`markup.inserted`);
            // the measured palette is the baseline it overrides.
            let scopes = crate::highlight::diff_scope_backgrounds();
            let pick = |rgb: Option<(u8, u8, u8)>, fallback: Color| match (rgb, l) {
                (Some(c), RichLevel::TrueColor) => rgb_color(c),
                (Some(c), RichLevel::Ansi256) => Color::Indexed(quantize_rgb_to_ansi256(c)),
                (None, _) => fallback,
            };
            DiffBackgrounds {
                add: Some(pick(scopes.inserted, add_line_bg(theme, l))),
                del: Some(pick(scopes.deleted, del_line_bg(theme, l))),
            }
        }
        None => DiffBackgrounds {
            add: None,
            del: None,
        },
    };
    StyleContext { theme, level, bgs }
}

fn rgb_color(c: (u8, u8, u8)) -> Color {
    Color::Rgb(c.0, c.1, c.2)
}

/// Nearest of the 240 fixed xterm colours (16 to 255) by squared RGB distance.
fn quantize_rgb_to_ansi256(c: (u8, u8, u8)) -> u8 {
    let cube = [0u8, 95, 135, 175, 215, 255];
    let mut best = (u32::MAX, 16u8);
    for i in 16u16..=255 {
        let (r, g, b) = if i < 232 {
            let n = (i - 16) as usize;
            (cube[n / 36], cube[(n / 6) % 6], cube[n % 6])
        } else {
            let v = 8 + 10 * (i - 232) as u8;
            (v, v, v)
        };
        let d = |a: u8, b: u8| (i32::from(a) - i32::from(b)).pow(2) as u32;
        let dist = d(r, c.0) + d(g, c.1) + d(b, c.2);
        if dist < best.0 {
            best = (dist, i as u8);
        }
    }
    best.1
}

fn add_line_bg(theme: DiffTheme, level: RichLevel) -> Color {
    match (theme, level) {
        (DiffTheme::Dark, RichLevel::TrueColor) => rgb(DARK_TC_ADD_LINE_BG_RGB),
        (DiffTheme::Dark, RichLevel::Ansi256) => Color::Indexed(DARK_256_ADD_LINE_BG_IDX),
        (DiffTheme::Light, RichLevel::TrueColor) => rgb(LIGHT_TC_ADD_LINE_BG_RGB),
        (DiffTheme::Light, RichLevel::Ansi256) => Color::Indexed(LIGHT_256_ADD_LINE_BG_IDX),
    }
}

fn del_line_bg(theme: DiffTheme, level: RichLevel) -> Color {
    match (theme, level) {
        (DiffTheme::Dark, RichLevel::TrueColor) => rgb(DARK_TC_DEL_LINE_BG_RGB),
        (DiffTheme::Dark, RichLevel::Ansi256) => Color::Indexed(DARK_256_DEL_LINE_BG_IDX),
        (DiffTheme::Light, RichLevel::TrueColor) => rgb(LIGHT_TC_DEL_LINE_BG_RGB),
        (DiffTheme::Light, RichLevel::Ansi256) => Color::Indexed(LIGHT_256_DEL_LINE_BG_IDX),
    }
}

fn light_gutter_fg(level: DiffColorLevel) -> Color {
    match level {
        DiffColorLevel::TrueColor => rgb(LIGHT_TC_GUTTER_FG_RGB),
        DiffColorLevel::Ansi256 => Color::Indexed(LIGHT_256_GUTTER_FG_IDX),
        DiffColorLevel::Ansi16 => Color::Black,
    }
}

fn line_bg_style(kind: LineKind, cx: &StyleContext) -> Style {
    let bg = match kind {
        LineKind::Insert => cx.bgs.add,
        LineKind::Delete => cx.bgs.del,
        LineKind::Context => None,
    };
    bg.map_or_else(Style::default, |bg| Style::default().bg(bg))
}

/// Line-number gutter: dim on dark terminals, an explicit dark foreground on light ones.
fn gutter_style(kind: LineKind, cx: &StyleContext) -> Style {
    match (cx.theme, kind, cx.level.rich()) {
        // Codex's source gives the light gutter its own background; the 0.147.0 captures show
        // the number on the row tint, so only the foreground is set.
        (DiffTheme::Light, LineKind::Insert | LineKind::Delete, _) => {
            Style::default().fg(light_gutter_fg(cx.level))
        }
        _ => Style::default().add_modifier(Modifier::DIM),
    }
}

/// Plain content style (no grammar): green or red over the tint on dark, tint only on light.
fn content_style(kind: LineKind, cx: &StyleContext) -> Style {
    let (fg, bg) = match kind {
        LineKind::Insert => (Color::Green, cx.bgs.add),
        LineKind::Delete => (Color::Red, cx.bgs.del),
        LineKind::Context => return Style::default(),
    };
    match (cx.theme, cx.level, bg) {
        (_, DiffColorLevel::Ansi16, _) => Style::default().fg(fg),
        (DiffTheme::Light, _, Some(bg)) => Style::default().bg(bg),
        (DiffTheme::Light, _, None) => Style::default(),
        (DiffTheme::Dark, _, Some(bg)) => Style::default().fg(fg).bg(bg),
        (DiffTheme::Dark, _, None) => Style::default().fg(fg),
    }
}

fn sign_style(kind: LineKind, cx: &StyleContext) -> Style {
    match (cx.theme, kind) {
        (DiffTheme::Light, LineKind::Insert) => Style::default().fg(Color::Green),
        (DiffTheme::Light, LineKind::Delete) => Style::default().fg(Color::Red),
        _ => content_style(kind, cx),
    }
}

// ---- model -----------------------------------------------------------------------------------

/// One row of a diff: its kind, the line number to show and the text.
struct DiffLine {
    kind: LineKind,
    number: usize,
    text: String,
}

type Hunk = Vec<DiffLine>;

fn trim_nl(s: &str) -> &str {
    s.strip_suffix('\n').unwrap_or(s)
}

/// Hunks of an edit with three lines of context, numbered from 1 within the snippet. An insert
/// shows its new number, a delete its old number and context the new number.
fn update_hunks(old: &str, new: &str) -> Vec<Hunk> {
    let diff = TextDiff::from_lines(old, new);
    let mut hunks = Vec::new();
    for group in diff.grouped_ops(CONTEXT_LINES) {
        let mut hunk = Vec::new();
        for op in &group {
            for ch in diff.iter_changes(op) {
                let (kind, number) = match ch.tag() {
                    ChangeTag::Insert => (LineKind::Insert, ch.new_index().unwrap_or(0) + 1),
                    ChangeTag::Delete => (LineKind::Delete, ch.old_index().unwrap_or(0) + 1),
                    ChangeTag::Equal => (LineKind::Context, ch.new_index().unwrap_or(0) + 1),
                };
                hunk.push(DiffLine {
                    kind,
                    number,
                    text: trim_nl(ch.value()).to_string(),
                });
            }
        }
        hunks.push(hunk);
    }
    hunks
}

fn line_counts(change: &Change) -> (usize, usize) {
    match change {
        Change::Add { content } => (content.lines().count(), 0),
        Change::Delete { content } => (0, content.lines().count()),
        Change::Update { old, new, .. } => {
            let diff = TextDiff::from_lines(old.as_str(), new.as_str());
            let mut added = 0;
            let mut removed = 0;
            for ch in diff.iter_all_changes() {
                match ch.tag() {
                    ChangeTag::Insert => added += 1,
                    ChangeTag::Delete => removed += 1,
                    ChangeTag::Equal => {}
                }
            }
            (added, removed)
        }
    }
}

// ---- summary block ---------------------------------------------------------------------------

fn render_line_count_summary(added: usize, removed: usize) -> Vec<Span<'static>> {
    vec![
        "(".into(),
        format!("+{added}").green(),
        " ".into(),
        format!("-{removed}").red(),
        ")".into(),
    ]
}

fn render_path(path: &str, change: &Change) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = vec![path.to_string().into()];
    if let Change::Update {
        move_to: Some(to), ..
    } = change
    {
        spans.push(format!(" \u{2192} {to}").into());
    }
    spans
}

fn render_changes_block(files: &[(String, Change)], width: usize) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    if files.is_empty() {
        return out;
    }
    let counts: Vec<(usize, usize)> = files.iter().map(|(_, c)| line_counts(c)).collect();
    let total_added: usize = counts.iter().map(|c| c.0).sum();
    let total_removed: usize = counts.iter().map(|c| c.1).sum();
    let file_count = files.len();

    let mut header: Vec<Span<'static>> = vec!["• ".dim()];
    if let [(path, change)] = files {
        let verb = match change {
            Change::Add { .. } => "Added",
            Change::Delete { .. } => "Deleted",
            Change::Update { .. } => "Edited",
        };
        header.push(verb.bold());
        header.push(" ".into());
        header.extend(render_path(path, change));
        header.push(" ".into());
        header.extend(render_line_count_summary(counts[0].0, counts[0].1));
    } else {
        let noun = if file_count == 1 { "file" } else { "files" };
        header.push("Edited".bold());
        header.push(format!(" {file_count} {noun} ").into());
        header.extend(render_line_count_summary(total_added, total_removed));
    }
    out.push(Line::from(header));

    let cx = context_for(&palette());
    for (idx, (path, change)) in files.iter().enumerate() {
        if idx > 0 {
            out.push(Line::default());
        }
        if file_count > 1 {
            let mut h: Vec<Span<'static>> = vec!["  └ ".dim()];
            h.extend(render_path(path, change));
            h.push(" ".into());
            h.extend(render_line_count_summary(counts[idx].0, counts[idx].1));
            out.push(Line::from(h));
        }
        // A rename highlights by the destination: the rows show the new file.
        let lang_path = match change {
            Change::Update {
                move_to: Some(to), ..
            } => to.as_str(),
            _ => path.as_str(),
        };
        let mut rows = Vec::new();
        render_change(
            change,
            &mut rows,
            width.saturating_sub(4).max(1),
            lang_path,
            &cx,
        );
        for mut row in rows {
            row.spans.insert(0, Span::raw("    "));
            out.push(row);
        }
    }
    out
}

/// Highlighted spans per line for `content`, by the grammar of `path`. A `.txt` file runs
/// through Plain Text, which still takes the theme's text colour.
fn path_spans(content: &str, path: &str) -> Option<Vec<Vec<Span<'static>>>> {
    let file = path.rsplit('/').next().unwrap_or(path);
    let ext = file.rsplit_once('.')?.1;
    match find_syntax(ext) {
        Some(sx) => Some(highlight_spans(content, Some(sx))),
        None if ext.eq_ignore_ascii_case("txt") => Some(highlight_spans(content, None)),
        None => None,
    }
}

fn exceeds_highlight_limits(bytes: usize, lines: usize) -> bool {
    bytes > MAX_HIGHLIGHT_BYTES || lines > MAX_HIGHLIGHT_LINES
}

fn render_change(
    change: &Change,
    out: &mut Vec<Line<'static>>,
    width: usize,
    lang_path: &str,
    cx: &StyleContext,
) {
    match change {
        Change::Add { content } | Change::Delete { content } => {
            let kind = if matches!(change, Change::Add { .. }) {
                LineKind::Insert
            } else {
                LineKind::Delete
            };
            let syntax = if exceeds_highlight_limits(content.len(), content.lines().count()) {
                None
            } else {
                path_spans(content, lang_path)
            };
            let w = line_number_width(content.lines().count());
            for (i, raw) in content.lines().enumerate() {
                let syn = syntax.as_ref().and_then(|s| s.get(i));
                out.extend(wrapped_diff_line(
                    i + 1,
                    kind,
                    raw,
                    width,
                    w,
                    syn.map(|v| v.as_slice()),
                    cx,
                ));
            }
        }
        Change::Update { old, new, .. } => {
            let hunks = update_hunks(old, new);
            let max_number = hunks.iter().flatten().map(|l| l.number).max().unwrap_or(0);
            let total_bytes: usize = hunks.iter().flatten().map(|l| l.text.len() + 1).sum();
            let total_lines: usize = hunks.iter().map(Vec::len).sum();
            let highlight = !exceeds_highlight_limits(total_bytes, total_lines);
            let w = line_number_width(max_number);
            for (i, hunk) in hunks.iter().enumerate() {
                if i > 0 {
                    let spacer = format!("{:width$} ", "", width = w.max(1));
                    out.push(Line::from(vec![
                        Span::styled(spacer, gutter_style(LineKind::Context, cx)),
                        "⋮".dim(),
                    ]));
                }
                // The hunk is one block so grammar state carries across its lines.
                let syntax = highlight
                    .then(|| {
                        let text: String = hunk.iter().map(|l| format!("{}\n", l.text)).collect();
                        path_spans(&text, lang_path).filter(|s| s.len() == hunk.len())
                    })
                    .flatten();
                for (j, l) in hunk.iter().enumerate() {
                    let syn = syntax.as_ref().and_then(|s| s.get(j));
                    out.extend(wrapped_diff_line(
                        l.number,
                        l.kind,
                        &l.text,
                        width,
                        w,
                        syn.map(|v| v.as_slice()),
                        cx,
                    ));
                }
            }
        }
    }
}

/// What a preview row stands for in the `/theme` picker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreviewKind {
    Context,
    Added,
    Removed,
}

/// The first row of one diff line as the active syntax theme and palette draw it, for the theme
/// preview (Codex `push_wrapped_diff_line_with_syntax_and_style_context`).
pub fn preview_diff_line(
    number: usize,
    kind: PreviewKind,
    text: &str,
    width: usize,
    number_width: usize,
    syntax: Option<&[Span<'static>]>,
) -> Line<'static> {
    let cx = context_for(&palette());
    let kind = match kind {
        PreviewKind::Context => LineKind::Context,
        PreviewKind::Added => LineKind::Insert,
        PreviewKind::Removed => LineKind::Delete,
    };
    wrapped_diff_line(number, kind, text, width, number_width, syntax, &cx)
        .into_iter()
        .next()
        .unwrap_or_default()
}

pub fn line_number_width(max_line_number: usize) -> usize {
    if max_line_number == 0 {
        1
    } else {
        max_line_number.to_string().len()
    }
}

/// One diff row wrapped to `width` columns: gutter, sign, then content; continuation rows have a
/// blank gutter and no sign. The tint is the line style so it reaches the right edge.
fn wrapped_diff_line(
    number: usize,
    kind: LineKind,
    text: &str,
    width: usize,
    number_width: usize,
    syntax: Option<&[Span<'static>]>,
    cx: &StyleContext,
) -> Vec<Line<'static>> {
    let gutter_width = number_width.max(1);
    let prefix_cols = gutter_width + 1;
    let sign_char = match kind {
        LineKind::Insert => '+',
        LineKind::Delete => '-',
        LineKind::Context => ' ',
    };
    let sign = sign_style(kind, cx);
    let gutter = gutter_style(kind, cx);
    let line_bg = line_bg_style(kind, cx);

    let styled: Vec<Span<'static>> = match syntax {
        Some(spans) => spans
            .iter()
            .map(|sp| {
                let style = if kind == LineKind::Delete {
                    sp.style.add_modifier(Modifier::DIM)
                } else {
                    sp.style
                };
                Span::styled(sp.content.to_string(), style)
            })
            .collect(),
        None => vec![Span::styled(text.to_string(), content_style(kind, cx))],
    };
    let avail = width.saturating_sub(prefix_cols + 1).max(1);
    let chunks = wrap_styled_spans(&styled, avail);
    let ln = number.to_string();
    chunks
        .into_iter()
        .enumerate()
        .map(|(i, chunk)| {
            let mut spans: Vec<Span<'static>> = Vec::new();
            if i == 0 {
                spans.push(Span::styled(format!("{ln:>gutter_width$} "), gutter));
                spans.push(Span::styled(sign_char.to_string(), sign));
            } else {
                spans.push(Span::styled(format!("{:gutter_width$}  ", ""), gutter));
            }
            spans.extend(chunk);
            Line::from(spans).style(line_bg)
        })
        .collect()
}

/// Split styled spans into rows of at most `max_cols` columns by character, tabs expanded to
/// four spaces. A character wider than the space left starts the next row.
fn wrap_styled_spans(spans: &[Span<'static>], max_cols: usize) -> Vec<Vec<Span<'static>>> {
    let mut result: Vec<Vec<Span<'static>>> = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut col = 0usize;
    for span in spans {
        let style = span.style;
        let mut remaining = span.content.as_ref();
        while !remaining.is_empty() {
            let mut byte_end = 0;
            let mut chars_col = 0;
            for ch in remaining.chars() {
                let w = ch.width().unwrap_or(if ch == '\t' { TAB_WIDTH } else { 0 });
                let w = if ch == '\t' { TAB_WIDTH } else { w };
                if col + chars_col + w > max_cols {
                    break;
                }
                byte_end += ch.len_utf8();
                chars_col += w;
            }
            if byte_end == 0 {
                if !current.is_empty() {
                    result.push(std::mem::take(&mut current));
                }
                let Some(ch) = remaining.chars().next() else {
                    break;
                };
                let len = ch.len_utf8();
                current.push(Span::styled(
                    remaining[..len].replace('\t', TAB_REPLACEMENT),
                    style,
                ));
                col = if ch == '\t' {
                    TAB_WIDTH
                } else {
                    ch.width().unwrap_or(1)
                };
                remaining = &remaining[len..];
                continue;
            }
            let (chunk, rest) = remaining.split_at(byte_end);
            current.push(Span::styled(chunk.replace('\t', TAB_REPLACEMENT), style));
            col += chars_col;
            remaining = rest;
            if col >= max_cols {
                result.push(std::mem::take(&mut current));
                col = 0;
            }
        }
    }
    if !current.is_empty() || result.is_empty() {
        result.push(current);
    }
    result
}

// ---- single-line cells (spec B.6.5) --------------------------------------------------------------

/// A cell that is just its lines, whatever the width.
#[derive(Clone, Debug)]
pub struct PlainCell {
    lines: Vec<Line<'static>>,
}

impl HistoryCell for PlainCell {
    fn display_lines(&self, _width: u16) -> Vec<Line<'static>> {
        self.lines.clone()
    }
}

/// `✘ Failed to apply patch` and, when there is stderr, its head and tail under `└`.
pub fn new_patch_apply_failure(stderr: &str) -> PlainCell {
    let mut lines: Vec<Line<'static>> =
        vec![Line::from("✘ Failed to apply patch".magenta().bold())];
    if !stderr.trim().is_empty() {
        let all: Vec<&str> = stderr.lines().collect();
        let total = all.len();
        let head_end = total.min(TOOL_CALL_MAX_LINES);
        let tail_len = (total - head_end).min(TOOL_CALL_MAX_LINES);
        let omitted = total - head_end - tail_len;
        let dim = |prefix: &str, text: &str| {
            Line::from(vec![
                Span::styled(
                    prefix.to_string(),
                    Style::default().add_modifier(Modifier::DIM),
                ),
                Span::styled(
                    text.to_string(),
                    Style::default().add_modifier(Modifier::DIM),
                ),
            ])
        };
        for (i, l) in all.iter().take(head_end).enumerate() {
            lines.push(dim(if i == 0 { "  └ " } else { "    " }, l));
        }
        if omitted > 0 {
            lines.push(dim(
                "    ",
                &format!("… +{omitted} lines (ctrl + t to view transcript)"),
            ));
        }
        for l in &all[total - tail_len..] {
            lines.push(dim("    ", l));
        }
    }
    PlainCell { lines }
}

/// `• Viewed Image` over `  └ path`, the path as given (already relative to the cwd).
pub fn new_view_image_tool_call(path: &str) -> PlainCell {
    PlainCell {
        lines: vec![
            Line::from(vec!["• ".dim(), "Viewed Image".bold()]),
            Line::from(vec!["  └ ".dim(), path.to_string().dim()]),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::{ColorLevel, Palette, set_palette};

    fn dark() {
        set_palette(Palette::new(
            Some((230, 230, 230)),
            Some((0, 0, 0)),
            ColorLevel::TrueColor,
        ));
    }

    fn light() {
        set_palette(Palette::new(
            Some((26, 26, 26)),
            Some((255, 255, 255)),
            ColorLevel::TrueColor,
        ));
    }

    fn plain(l: &Line<'_>) -> String {
        l.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn text(cell: &dyn HistoryCell, w: u16) -> Vec<String> {
        cell.display_lines(w).iter().map(plain).collect()
    }

    fn update(path: &str, old: &str, new: &str) -> (String, Change) {
        (
            path.into(),
            Change::Update {
                old: old.into(),
                new: new.into(),
                move_to: None,
            },
        )
    }

    // ---- ported Codex snapshots -------------------------------------------------------------

    #[test]
    fn apply_update_block() {
        dark();
        let c = PatchCell::new(vec![update(
            "example.txt",
            "line one\nline two\nline three\n",
            "line one\nline two changed\nline three\n",
        )]);
        assert_eq!(
            text(&c, 80),
            vec![
                "• Edited example.txt (+1 -1)",
                "    1  line one",
                "    2 -line two",
                "    2 +line two changed",
                "    3  line three",
            ]
        );
    }

    #[test]
    fn apply_add_block() {
        dark();
        let c = PatchCell::new(vec![(
            "new_file.txt".into(),
            Change::Add {
                content: "alpha\nbeta\n".into(),
            },
        )]);
        assert_eq!(
            text(&c, 80),
            vec![
                "• Added new_file.txt (+2 -0)",
                "    1 +alpha",
                "    2 +beta"
            ]
        );
    }

    #[test]
    fn apply_delete_block() {
        dark();
        let c = PatchCell::new(vec![(
            "tmp_delete_example.txt".into(),
            Change::Delete {
                content: "first\nsecond\nthird\n".into(),
            },
        )]);
        assert_eq!(
            text(&c, 80),
            vec![
                "• Deleted tmp_delete_example.txt (+0 -3)",
                "    1 -first",
                "    2 -second",
                "    3 -third",
            ]
        );
    }

    #[test]
    fn apply_multiple_files_block() {
        dark();
        let c = PatchCell::new(vec![
            (
                "b.txt".into(),
                Change::Add {
                    content: "new\n".into(),
                },
            ),
            update("a.txt", "one\n", "one changed\n"),
        ]);
        assert_eq!(
            text(&c, 80),
            vec![
                "• Edited 2 files (+2 -1)",
                "  └ a.txt (+1 -1)",
                "    1 -one",
                "    1 +one changed",
                "",
                "  └ b.txt (+1 -0)",
                "    1 +new",
            ]
        );
    }

    #[test]
    fn apply_update_with_rename_block() {
        dark();
        let c = PatchCell::new(vec![(
            "old_name.rs".into(),
            Change::Update {
                old: "A\nB\nC\n".into(),
                new: "A\nB changed\nC\n".into(),
                move_to: Some("new_name.rs".into()),
            },
        )]);
        assert_eq!(
            text(&c, 80),
            vec![
                "• Edited old_name.rs → new_name.rs (+1 -1)",
                "    1  A",
                "    2 -B",
                "    2 +B changed",
                "    3  C",
            ]
        );
    }

    #[test]
    fn apply_update_block_wraps_long_lines_text() {
        dark();
        let c = PatchCell::new(vec![update(
            "wrap_demo.txt",
            "1\n2\n3\n4\n",
            "1\nadded long line which wraps and_if_there_is_a_long_token_it_will_be_broken\n3\n4 context line which also wraps across\n",
        )]);
        // The row that fills the width keeps its trailing space; the snapshot tool trims it.
        let rows: Vec<String> = text(&c, 28)
            .iter()
            .map(|r| r.trim_end().to_string())
            .collect();
        assert_eq!(
            rows,
            vec![
                "• Edited wrap_demo.txt (+2 -2)",
                "    1  1",
                "    2 -2",
                "    2 +added long line which",
                "        wraps and_if_there_i",
                "       s_a_long_token_it_wil",
                "       l_be_broken",
                "    3  3",
                "    4 -4",
                "    4 +4 context line which",
                "       also wraps across",
            ]
        );
    }

    #[test]
    fn apply_update_block_line_numbers_three_digits_text() {
        dark();
        let old: String = (97..=101).map(|i| format!("line {i}\n")).collect();
        let new: String = (97..=101)
            .map(|i| {
                if i == 100 {
                    "line 100 changed\n".to_string()
                } else {
                    format!("line {i}\n")
                }
            })
            .collect();
        // Snippet numbering starts at 1, so pad the front to land on 97.
        let pad: String = (1..=96).map(|i| format!("p{i}\n")).collect();
        let c = PatchCell::new(vec![update(
            "hundreds.txt",
            &format!("{pad}{old}"),
            &format!("{pad}{new}"),
        )]);
        assert_eq!(
            text(&c, 80),
            vec![
                "• Edited hundreds.txt (+1 -1)",
                "     97  line 97",
                "     98  line 98",
                "     99  line 99",
                "    100 -line 100",
                "    100 +line 100 changed",
                "    101  line 101",
            ]
        );
    }

    #[test]
    fn vertical_ellipsis_between_hunks() {
        dark();
        let old: String = (1..=40).map(|i| format!("l{i}\n")).collect();
        let new = old.replace("l5\n", "l5x\n").replace("l30\n", "l30x\n");
        let rows = text(&PatchCell::new(vec![update("a.txt", &old, &new)]), 80);
        let at = rows.iter().position(|r| r.contains('⋮')).unwrap();
        assert_eq!(rows[at], "       ⋮");
        assert_eq!(rows[at - 1], "     8  l8");
        assert_eq!(rows[at + 1], "    27  l27");
    }

    #[test]
    fn gallery_adds_with_tabs_and_wide_chars_at_80() {
        dark();
        let c = PatchCell::new(vec![
            (
                "assets/banner.txt".into(),
                Change::Add {
                    content: "HEADER\tVALUE\nrocket\t🚀\ncity\t東京\n".into(),
                },
            ),
            update(
                "scripts/calc.txt",
                "def add(a, b):\n    return a + b\n\nprint(add(1, 2))\n",
                "def add(a, b):\n    return a + b + 42\n\nprint(add(1, 2))\n",
            ),
        ]);
        let rows = text(&c, 80);
        assert_eq!(rows[0], "• Edited 2 files (+4 -1)");
        assert_eq!(rows[1], "  └ assets/banner.txt (+3 -0)");
        assert_eq!(rows[2], "    1 +HEADER    VALUE");
        assert_eq!(rows[3], "    2 +rocket    🚀");
        assert_eq!(rows[4], "    3 +city    東京");
        assert_eq!(rows[6], "  └ scripts/calc.txt (+1 -1)");
        assert_eq!(rows[7], "    1  def add(a, b):");
        assert_eq!(rows[10], "    3  ");
    }

    // ---- reference captures -----------------------------------------------------------------

    #[test]
    fn matches_turns_06_patch_done() {
        dark();
        let c = PatchCell::new(vec![update(
            "src/lib.rs",
            "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
            "pub fn add(a: i32, b: i32) -> i32 {\n    a.wrapping_add(b)\n}\n",
        )]);
        assert_eq!(
            text(&c, 120),
            vec![
                "• Edited src/lib.rs (+1 -1)",
                "    1  pub fn add(a: i32, b: i32) -> i32 {",
                "    2 -    a + b",
                "    2 +    a.wrapping_add(b)",
                "    3  }",
            ]
        );
    }

    /// Cells as `[fg bg mods]text` runs for the span styles after the row style is applied.
    fn runs(l: &Line<'_>) -> String {
        let mut out = String::new();
        for sp in &l.spans {
            let st = l.style.patch(sp.style);
            let col = |c: Option<Color>| match c {
                Some(Color::Rgb(r, g, b)) => format!("{r:02x}{g:02x}{b:02x}"),
                Some(Color::Green) => "green".into(),
                Some(Color::Red) => "red".into(),
                Some(Color::Indexed(i)) => format!("d{i}"),
                Some(c) => format!("{c:?}"),
                None => "-".into(),
            };
            let mut m = String::new();
            if st.add_modifier.contains(Modifier::DIM) {
                m.push('d');
            }
            out.push_str(&format!(
                "[{}/{} {}]{}",
                col(st.fg),
                col(st.bg),
                m,
                sp.content
            ));
        }
        out
    }

    #[test]
    fn turns_06_row_colours_dark() {
        dark();
        let c = PatchCell::new(vec![update(
            "src/lib.rs",
            "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
            "pub fn add(a: i32, b: i32) -> i32 {\n    a.wrapping_add(b)\n}\n",
        )]);
        let rows = c.display_lines(120);
        // Context row: no background, dim number.
        assert!(runs(&rows[1]).starts_with("[-/- ]    [-/- d]1 [-/- ] "));
        // Delete row: dim gutter on the red tint, red sign, dim Text colour content.
        let del = runs(&rows[2]);
        assert!(
            del.starts_with("[-/4a221d ]    [-/4a221d d]2 [red/4a221d ]-"),
            "{del}"
        );
        assert!(del.contains("[cdd6f4/4a221d d]"), "{del}");
        assert!(del.contains("[94e2d5/4a221d d]+"), "{del}");
        // Add row: green sign, syntax colours without dim, all on the green tint.
        let add = runs(&rows[3]);
        assert!(
            add.starts_with("[-/213a2b ]    [-/213a2b d]2 [green/213a2b ]+"),
            "{add}"
        );
        assert!(add.contains("[94e2d5/213a2b ]."), "{add}");
        assert!(add.contains("[89b4fa/213a2b ]wrapping_add"), "{add}");
        assert_eq!(rows[2].style.bg, Some(Color::Rgb(74, 34, 29)));
    }

    #[test]
    fn turns_06_row_colours_light() {
        light();
        let c = PatchCell::new(vec![update(
            "src/lib.rs",
            "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
            "pub fn add(a: i32, b: i32) -> i32 {\n    a.wrapping_add(b)\n}\n",
        )]);
        let rows = c.display_lines(120);
        let add = runs(&rows[3]);
        // Gutter keeps the dark foreground on the row tint (the capture shows no separate gutter bg).
        assert!(add.contains("[1f2328/dafbe1 ]2 "), "{add}");
        assert_eq!(rows[3].style.bg, Some(Color::Rgb(218, 251, 225)));
        assert_eq!(rows[2].style.bg, Some(Color::Rgb(255, 235, 233)));
        set_palette(Palette::default());
    }

    #[test]
    fn sixteen_colour_terminal_has_no_tint() {
        set_palette(Palette::new(None, None, ColorLevel::Ansi16));
        let c = PatchCell::new(vec![update("a.txt", "x\n", "y\n")]);
        let rows = c.display_lines(40);
        assert_eq!(rows[1].style.bg, None);
        assert_eq!(rows[1].spans[2].style.fg, Some(Color::Red));
        assert_eq!(rows[2].spans[2].style.fg, Some(Color::Green));
        set_palette(Palette::default());
    }

    #[test]
    fn xterm_256_uses_palette_indices() {
        set_palette(Palette::new(
            Some((230, 230, 230)),
            Some((0, 0, 0)),
            ColorLevel::Ansi256,
        ));
        let c = PatchCell::new(vec![update("a.txt", "x\n", "y\n")]);
        let rows = c.display_lines(40);
        assert_eq!(rows[1].style.bg, Some(Color::Indexed(52)));
        assert_eq!(rows[2].style.bg, Some(Color::Indexed(22)));
        set_palette(Palette::default());
    }

    #[test]
    fn multi_file_capture_shape() {
        dark();
        // Sorted with uppercase first, a delete shows `+0 -1` and an add `+5 -0`.
        let c = PatchCell::new(vec![
            (
                "notes.txt".into(),
                Change::Delete {
                    content: "scratch notes\n".into(),
                },
            ),
            update("src/lib.rs", "a\n", "b\n"),
            update("README.md", "# demo project\nhello world\nthis is line three\n", "# demo project\nhello, wrapped world\nthis is line three\n"),
            (
                "src/util.rs".into(),
                Change::Add {
                    content: "//! Small helpers.\n\npub fn clamp(x: i32, lo: i32, hi: i32) -> i32 {\n    x.max(lo).min(hi)\n}\n".into(),
                },
            ),
        ]);
        let rows = text(&c, 150);
        assert_eq!(rows[0], "• Edited 4 files (+7 -3)");
        assert_eq!(rows[1], "  └ README.md (+1 -1)");
        let heads: Vec<&String> = rows.iter().filter(|r| r.starts_with("  └ ")).collect();
        assert_eq!(
            heads,
            vec![
                "  └ README.md (+1 -1)",
                "  └ notes.txt (+0 -1)",
                "  └ src/lib.rs (+1 -1)",
                "  └ src/util.rs (+5 -0)"
            ]
        );
        // The `.txt` delete takes the theme text colour, dim, not red.
        dark();
        let del_row = c
            .display_lines(150)
            .into_iter()
            .find(|l| plain(l).contains("scratch notes"))
            .unwrap();
        assert!(runs(&del_row).contains("[cdd6f4/4a221d d]scratch notes"));
    }

    #[test]
    fn long_added_line_wraps_by_columns_at_every_width() {
        dark();
        let long = "a very long added line that keeps going ".repeat(6);
        for w in [30u16, 40, 60, 80, 120, 150, 200] {
            let c = PatchCell::new(vec![update(
                "README.md",
                "# demo project\nhello world\n",
                &format!("# demo project\n{long}\ntab\there\nhello world\n"),
            )]);
            let lines = c.display_lines(w);
            for l in &lines {
                let width: usize = l
                    .spans
                    .iter()
                    .map(|s| crate::wrap::width_of(&s.content))
                    .sum();
                assert!(width <= w as usize, "w={w} row {:?}", plain(l));
            }
            let rows: Vec<String> = lines.iter().map(plain).collect();
            // First added row: 4 spaces, gutter `2 `, sign, then width - 4 - 2 - 1 content columns.
            let first = rows.iter().find(|r| r.starts_with("    2 +")).unwrap();
            let want = (w as usize - 4 - 2 - 1).min(long.trim_end().len());
            assert_eq!(first.len() - 7, want, "w={w}");
            // Continuation rows have W + 2 blanks after the prefix and no sign.
            if let Some(i) = rows.iter().position(|r| r.starts_with("    2 +")) {
                if long.len() > want {
                    assert!(rows[i + 1].starts_with("       "), "w={w}");
                }
            }
            assert!(rows.iter().any(|r| r.contains("tab    here")), "w={w}");
        }
    }

    #[test]
    fn wide_char_never_splits() {
        dark();
        let c = PatchCell::new(vec![(
            "a.txt".into(),
            Change::Add {
                content: "東京東京東京\n".into(),
            },
        )]);
        let rows = text(&c, 11);
        // width 11 - 4 = 7 block, 1 digit gutter + space + sign + 1 margin = 4 left, so two wide chars a row.
        assert_eq!(rows[1], "    1 +東京");
        assert_eq!(rows[2], "       東京");
    }

    #[test]
    fn hunk_gap_row_has_no_tint() {
        dark();
        let old: String = (1..=40).map(|i| format!("l{i}\n")).collect();
        let new = old.replace("l5\n", "l5x\n").replace("l30\n", "l30x\n");
        let c = PatchCell::new(vec![update("a.txt", &old, &new)]);
        let rows = c.display_lines(80);
        let gap = rows.iter().find(|l| plain(l).contains('⋮')).unwrap();
        assert_eq!(gap.style.bg, None);
        assert_eq!(runs(gap), "[-/- ]    [-/- d]   [-/- d]⋮");
    }

    #[test]
    fn failure_cell_has_title_and_dim_stderr() {
        let c = new_patch_apply_failure("error: could not apply\nsecond line");
        let rows = text(&c, 80);
        assert_eq!(
            rows,
            vec![
                "✘ Failed to apply patch",
                "  └ error: could not apply",
                "    second line"
            ]
        );
        let l = c.display_lines(80);
        assert_eq!(l[0].spans[0].style.fg, Some(Color::Magenta));
        assert!(new_patch_apply_failure("  \n").display_lines(80).len() == 1);
        let many: String = (1..=30).map(|i| format!("e{i}\n")).collect();
        let rows = text(&new_patch_apply_failure(&many), 80);
        assert_eq!(rows.len(), 1 + 5 + 1 + 5);
        assert_eq!(rows[6], "    … +20 lines (ctrl + t to view transcript)");
    }

    #[test]
    fn view_image_cell() {
        let c = new_view_image_tool_call("img.png");
        assert_eq!(text(&c, 80), vec!["• Viewed Image", "  └ img.png"]);
    }

    #[test]
    fn empty_cell_has_no_rows() {
        assert!(PatchCell::new(Vec::new()).display_lines(80).is_empty());
    }
}
