//! Ratatui rendering: pure functions from [`App`] state to widgets.
//! Layout: chat transcript above the input line and a quiet status line.
//! Floating layers: the command-suggestion popup and the mode picker.
//!
//! Design rules (do not regress):
//! - **Transparent**: never paint a background color; everything renders on
//!   `Color::Reset` so the user's terminal background shows through.
//!   Selection reads through an accent marker + bold, not opaque slabs.
//! - **Monochrome**: white accent plus dim grays only — no hues anywhere.
//!   Emphasis reads through brightness and bold, semantics through glyphs
//!   (✓/✗), never color.
//! - **No heavy boxes**: borderless sections separated by padding and dim
//!   rules; rounded dim borders only on floating layers.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Mutex, OnceLock};

use pulldown_cmark::{CodeBlockKind, Event as MdEvent, Options, Parser, Tag, TagEnd};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Margin, Position, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, BorderType, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Theme, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{App, InputMode, TranscriptEntry};
use crate::config::Mode;

const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// The single accent color used for chrome (prompt, gutters, names,
/// attention borders).
const ACCENT: Color = Color::White;
/// Dim chrome: rules, gutter marks, hints, secondary borders.
const DIM: Color = Color::DarkGray;
/// Secondary text (tool output, user echo, details).
const TEXT_DIM: Color = Color::Gray;
/// Inline code (block code gets grayscale syntect foregrounds, or
/// [`TEXT_DIM`] when plain).
const CODE: Color = Color::White;

fn dim() -> Style {
    Style::default().fg(DIM)
}

fn accent() -> Style {
    Style::default().fg(ACCENT)
}

/// Render one frame. The only entry point the main loop calls; everything
/// else in this module is a helper.
pub fn draw(frame: &mut Frame, app: &App) {
    let [main_area, input_area, status_area] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(2),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    draw_transcript(frame, app, main_area);
    draw_input(frame, app, input_area);
    draw_status_bar(frame, app, status_area);

    // Floating layers, back to front.
    if app.picker.is_none() {
        draw_suggestions(frame, app, input_area);
    }
    if app.picker.is_some() {
        draw_picker(frame, app);
    }
}

/// Chat transcript: user/assistant messages with streaming markdown and
/// collapsible tool cards. Borderless; a one-column side margin keeps the
/// text off the terminal edge. Shows the welcome screen while empty.
fn draw_transcript(frame: &mut Frame, app: &App, area: Rect) {
    if app.transcript.is_empty() && app.streaming.is_empty() && !app.status.busy {
        draw_welcome(frame, app, area);
        return;
    }

    let inner = area.inner(Margin {
        horizontal: 1,
        vertical: 0,
    });
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let inner_width = inner.width as usize;
    let inner_height = inner.height as usize;

    let lines = wrap_lines(transcript_text(app), inner_width);
    let total = lines.len();
    let max_scroll = total.saturating_sub(inner_height);
    let scroll = (app.scroll as usize).min(max_scroll);
    let start = max_scroll - scroll;
    let end = (start + inner_height).min(total);
    let visible: Vec<Line<'static>> = lines[start..end].to_vec();

    frame.render_widget(Paragraph::new(Text::from(visible)), inner);

    // Scrolled away from the tail: a quiet hint in the top-right corner.
    if scroll > 0 {
        let label = format!("↓ {scroll} more ");
        let width = (label.width() as u16).min(inner.width);
        let hint = Rect {
            x: inner.right().saturating_sub(width),
            y: inner.y,
            width,
            height: 1,
        };
        frame.render_widget(Clear, hint);
        frame.render_widget(Paragraph::new(Span::styled(label, dim())), hint);
    }

    // A whisper of a scrollbar in the right margin once content overflows.
    if total > inner_height {
        let mut state = ScrollbarState::new(max_scroll + 1).position(start);
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .track_symbol(None)
            .thumb_symbol("▐")
            .thumb_style(dim());
        frame.render_stateful_widget(scrollbar, area, &mut state);
    }
}

/// Welcome screen shown before the first message: a small centered card,
/// no borders, no banner art.
fn draw_welcome(frame: &mut Frame, app: &App, area: Rect) {
    let lines: Vec<Line<'static>> = vec![
        Line::from(Span::styled("✦", accent())),
        Line::raw(""),
        Line::from(Span::styled(
            "w i z a r d",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "your sovereign agent — self-extending, fully local",
            dim().italic(),
        )),
        Line::raw(""),
        Line::from(vec![
            Span::styled(app.status.model.clone(), Style::default().fg(TEXT_DIM)),
            Span::styled(" · ", dim()),
            mode_span(app.status.mode),
        ]),
        Line::raw(""),
        Line::raw(""),
        Line::from(vec![
            Span::styled("type a message", Style::default().fg(TEXT_DIM)),
            Span::styled(" and press Enter to begin", dim()),
        ]),
        Line::raw(""),
        Line::from(vec![
            Span::styled("/", accent()),
            Span::styled("  commands — Tab completes, ↑/↓ select", dim()),
        ]),
        Line::from(vec![
            Span::styled("/model", accent()),
            Span::styled("  show or switch the model", dim()),
        ]),
        Line::from(vec![
            Span::styled("/help", accent()),
            Span::styled("  all commands & keys", dim()),
        ]),
    ];

    let height = lines.len() as u16;
    let top = area.height.saturating_sub(height) / 2;
    let centered = Rect {
        x: area.x,
        y: area.y + top,
        width: area.width,
        height: height.min(area.height),
    };
    frame.render_widget(
        Paragraph::new(Text::from(lines)).alignment(Alignment::Center),
        centered,
    );
}

/// Colored span for a mode name: genie is quiet, sovereign is a warning.
fn mode_span(mode: Mode) -> Span<'static> {
    match mode {
        Mode::Genie => Span::styled("genie", Style::default().fg(TEXT_DIM)),
        Mode::Sovereign => Span::styled("sovereign", Style::default().fg(Color::White).bold()),
    }
}

/// Prefix a rendered block with a gutter: `marker` on the first line, a
/// two-column indent on the rest, so the message hangs off its mark.
fn gutter_block(lines: &mut Vec<Line<'static>>, text: Text<'static>, marker: Span<'static>) {
    for (index, mut line) in text.lines.into_iter().enumerate() {
        let mut spans = Vec::with_capacity(line.spans.len() + 1);
        if index == 0 {
            spans.push(marker.clone());
        } else if !line.spans.is_empty() {
            spans.push(Span::raw("  "));
        }
        spans.append(&mut line.spans);
        lines.push(Line::from(spans));
    }
}

/// Render model reasoning ("thinking") as plain dimmed-italic lines.
/// No markdown: reasoning is background noise, not the answer.
fn thinking_text(message: &str) -> Text<'static> {
    let style = dim().italic();
    let lines: Vec<Line<'static>> = message
        .lines()
        .map(|line| Line::from(Span::styled(line.to_string(), style)))
        .collect();
    Text::from(lines)
}

/// Build the full (unwrapped) transcript text from app state.
fn transcript_text(app: &App) -> Text<'static> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut prev_tool = false;
    let mut prev_notice = false;
    let mut first = true;

    for entry in &app.transcript {
        let is_tool = matches!(entry, TranscriptEntry::ToolCard { .. });
        let is_notice = matches!(entry, TranscriptEntry::Notice(_));
        // Comfortable spacing between turns; runs of tool cards or notices
        // stay tight so they read as one group.
        let tight = (is_tool && prev_tool) || (is_notice && prev_notice);
        if !first && !tight {
            lines.push(Line::raw(""));
        }
        first = false;
        prev_tool = is_tool;
        prev_notice = is_notice;

        match entry {
            TranscriptEntry::User(message) => {
                let mut user_lines: Vec<Line<'static>> = Vec::new();
                for line in message.lines() {
                    user_lines.push(Line::from(Span::styled(
                        line.to_string(),
                        Style::default().fg(TEXT_DIM),
                    )));
                }
                gutter_block(
                    &mut lines,
                    Text::from(user_lines),
                    Span::styled("❯ ", dim().bold()),
                );
            }
            TranscriptEntry::Assistant(message) => {
                gutter_block(
                    &mut lines,
                    render_markdown(message),
                    Span::styled("· ", accent()),
                );
            }
            TranscriptEntry::Thinking(message) => {
                gutter_block(
                    &mut lines,
                    thinking_text(message),
                    Span::styled("· ", dim()),
                );
            }
            TranscriptEntry::ToolCard {
                name,
                args,
                output,
                is_error,
                collapsed,
            } => {
                tool_card_lines(
                    &mut lines,
                    name,
                    args,
                    output.as_deref(),
                    *is_error,
                    *collapsed,
                    app.tick,
                );
            }
            TranscriptEntry::Notice(message) => {
                let style = if message.starts_with("error") {
                    Style::default().fg(Color::White).bold()
                } else {
                    dim().italic()
                };
                for line in message.lines() {
                    lines.push(Line::from(Span::styled(format!("  {line}"), style)));
                }
            }
        }
    }

    if !app.streaming_thinking.is_empty() {
        if !first {
            lines.push(Line::raw(""));
        }
        first = false;
        // In-flight reasoning, dimmed so it reads as background noise.
        gutter_block(
            &mut lines,
            thinking_text(&app.streaming_thinking),
            Span::styled("· ", dim()),
        );
    }
    if !app.streaming.is_empty() {
        if !first {
            lines.push(Line::raw(""));
        }
        // Streaming: the text itself arriving, with a soft cursor at the
        // tail. Code blocks stay unhighlighted while in flight (cheap to
        // re-render every frame).
        let mut text = render_markdown_streaming(&app.streaming);
        let tail = Span::styled("▍", dim());
        match text.lines.last_mut() {
            Some(last) => last.spans.push(tail),
            None => text.lines.push(Line::from(tail)),
        }
        gutter_block(&mut lines, text, Span::styled("· ", accent()));
    } else if app.status.busy {
        if !first {
            lines.push(Line::raw(""));
        }
        let spinner = SPINNER[(app.tick as usize) % SPINNER.len()];
        lines.push(Line::from(vec![
            Span::styled(format!("{spinner} "), accent()),
            Span::styled(format!("{}…", app.spinner_verb), dim().italic()),
        ]));
    }

    Text::from(lines)
}

/// Render one tool invocation as a compact single-line card: status glyph,
/// tool name in accent, truncated args in dim. Output expands below only
/// when relevant (errors, or Ctrl-T).
fn tool_card_lines(
    lines: &mut Vec<Line<'static>>,
    name: &str,
    args: &serde_json::Value,
    output: Option<&str>,
    is_error: bool,
    collapsed: bool,
    tick: u64,
) {
    const MAX_OUTPUT_LINES: usize = 200;

    let glyph = match (output, is_error) {
        (None, _) => Span::styled(
            SPINNER[(tick as usize) % SPINNER.len()].to_string(),
            accent(),
        ),
        (Some(_), false) => Span::styled("✓", Style::default().fg(TEXT_DIM)),
        (Some(_), true) => Span::styled("✗", Style::default().fg(Color::White).bold()),
    };

    let summary = if args.is_null() {
        String::new()
    } else {
        truncate_width(&serde_json::to_string(args).unwrap_or_default(), 64)
    };
    let mut card = vec![
        glyph,
        Span::raw(" "),
        Span::styled(name.to_string(), accent()),
    ];
    if !summary.is_empty() {
        card.push(Span::styled(format!("  {summary}"), dim()));
    }
    let hidden = output.map(|text| text.lines().count()).unwrap_or(0);
    if collapsed && hidden > 0 {
        card.push(Span::styled(format!("  +{hidden} lines"), dim().italic()));
    }
    lines.push(Line::from(card));

    if !collapsed && let Some(text) = output {
        let body = Style::default().fg(TEXT_DIM);
        let out_lines: Vec<&str> = text.lines().collect();
        for line in out_lines.iter().take(MAX_OUTPUT_LINES) {
            lines.push(Line::from(Span::styled(format!("  {line}"), body)));
        }
        if out_lines.len() > MAX_OUTPUT_LINES {
            lines.push(Line::from(Span::styled(
                format!("  … +{} lines", out_lines.len() - MAX_OUTPUT_LINES),
                dim(),
            )));
        }
    }
}

/// Bottom status line: model, mode, and turn state on the left; contextual
/// key hints on the right. One quiet line, no background fill.
fn draw_status_bar(frame: &mut Frame, app: &App, area: Rect) {
    let spinner = SPINNER[(app.tick as usize) % SPINNER.len()];
    let mut spans = vec![
        Span::styled(" ✦ ", accent()),
        Span::styled(app.status.model.clone(), Style::default().fg(TEXT_DIM)),
        Span::styled(" · ", dim()),
        mode_span(app.status.mode),
    ];
    if app.status.busy {
        let elapsed = app
            .turn_started
            .map(|started| started.elapsed().as_secs())
            .unwrap_or(0);
        spans.push(Span::styled(" · ", dim()));
        spans.push(Span::styled(format!("{spinner} "), accent()));
        spans.push(Span::styled(
            format!("step {} · {elapsed}s", app.status.step),
            dim(),
        ));
    }
    let line = Line::from(spans);
    let left_width = line.width() as u16;
    frame.render_widget(Paragraph::new(line), area);

    // Contextual key hints, right-aligned in a sub-rect so the left side is
    // never overdrawn.
    let hints = if app.picker.is_some() {
        "↑↓ move · Enter select · Esc cancel"
    } else if !app.suggestions.is_empty() {
        "↑↓ select · Tab complete · Enter run"
    } else if app.status.busy {
        "PgUp/PgDn scroll · ^C quit"
    } else {
        "/ commands · ↑ history · ^C quit"
    };
    let width = hints.width() as u16 + 1;
    if area.width > left_width + width {
        let hint_area = Rect {
            x: area.right().saturating_sub(width),
            y: area.y,
            width,
            height: 1,
        };
        frame.render_widget(Paragraph::new(Span::styled(hints, dim())), hint_area);
    }
}

/// Input: a dim rule above a clean accent prompt — no box. Handles
/// cursor-aware horizontal scrolling and inline ghost-text completion.
fn draw_input(frame: &mut Frame, app: &App, area: Rect) {
    if area.height < 2 || area.width < 6 {
        return;
    }
    let rule = Line::from(Span::styled("─".repeat(area.width as usize), dim()));

    // One column of left padding keeps the prompt aligned with the
    // transcript margin.
    let pad = 1usize;
    let prompt_width = 2usize;
    let budget = (area.width as usize)
        .saturating_sub(pad + prompt_width + 1)
        .max(1);

    let chars: Vec<char> = app.input.chars().collect();
    let widths: Vec<usize> = chars.iter().map(|c| c.width().unwrap_or(0)).collect();
    let cursor = app.cursor.min(chars.len());
    // Keep the cursor visible: scroll the window (in display columns, so
    // wide CJK/emoji glyphs count properly) until the cursor column fits,
    // truncating the tail if needed.
    let mut start = 0usize;
    let mut cursor_cols: usize = widths[..cursor].iter().sum();
    while start < cursor && cursor_cols > budget - 1 {
        cursor_cols -= widths[start];
        start += 1;
    }
    let mut end = start;
    let mut used_cols = 0usize;
    while end < chars.len() && used_cols + widths[end] <= budget {
        used_cols += widths[end];
        end += 1;
    }
    let visible: String = chars[start..end].iter().collect();
    let cursor_x = area.x + (pad + prompt_width) as u16 + cursor_cols as u16;

    let mut spans = vec![
        Span::raw(" "),
        Span::styled("❯ ", accent().bold()),
        Span::raw(visible),
    ];

    // Ghost text: the untyped remainder of the highlighted suggestion plus
    // its argument hint, dimmed (only when the whole input is visible and
    // the cursor sits at the end, where → can actually accept it).
    if start == 0
        && cursor == chars.len()
        && app.picker.is_none()
        && app.input_mode == InputMode::Command
        && let Some(spec) = app.suggestions.get(app.suggestion_index)
    {
        let typed = app.input.trim_start().strip_prefix('/').unwrap_or_default();
        if let Some(remainder) = spec.name.strip_prefix(typed) {
            let mut ghost = remainder.to_string();
            if !spec.args.is_empty() {
                ghost.push(' ');
                ghost.push_str(&spec.args);
            }
            let room = budget.saturating_sub(used_cols);
            if !ghost.is_empty() && room > 0 {
                let ghost: String = ghost.chars().take(room).collect();
                spans.push(Span::styled(ghost, dim().italic()));
            }
        }
    }

    frame.render_widget(
        Paragraph::new(Text::from(vec![rule, Line::from(spans)])),
        area,
    );

    if app.picker.is_none() {
        frame.set_cursor_position(Position::new(cursor_x, area.y + 1));
    }
}

/// Command-suggestion popup floating directly above the input rule.
fn draw_suggestions(frame: &mut Frame, app: &App, input_area: Rect) {
    if app.suggestions.is_empty() {
        return;
    }

    let rows = app.suggestions.len() as u16;
    let bottom = input_area.y;
    let height = (rows + 2).min(bottom);
    let area = Rect {
        x: input_area.x,
        y: bottom.saturating_sub(height),
        width: input_area.width,
        height,
    }
    .intersection(frame.area());
    if area.height < 3 || area.width < 4 {
        return;
    }
    frame.render_widget(Clear, area);

    let usage_width = app
        .suggestions
        .iter()
        .map(|spec| spec.name.len() + spec.args.len() + 2)
        .max()
        .unwrap_or(0);
    let inner_width = area.width.saturating_sub(2) as usize;
    // Columns left for the description: marker + padded usage + gap.
    let description_room = inner_width.saturating_sub(usage_width + 5);

    // Window the rows so the ❯ selection stays visible on short terminals
    // (selection pinned to the bottom edge while moving down).
    let visible_rows = area.height.saturating_sub(2) as usize;
    let start = if app.suggestion_index >= visible_rows {
        app.suggestion_index + 1 - visible_rows
    } else {
        0
    };

    let lines: Vec<Line<'static>> = app
        .suggestions
        .iter()
        .enumerate()
        .skip(start)
        .take(visible_rows)
        .map(|(index, spec)| {
            let selected = index == app.suggestion_index;
            let (marker, name_style) = if selected {
                ("❯ ", accent().bold())
            } else {
                ("  ", Style::default().fg(TEXT_DIM))
            };
            let usage = format!("/{} {}", spec.name, spec.args);
            Line::from(vec![
                Span::styled(marker, accent()),
                Span::styled(format!("{usage:<usage_width$}"), name_style),
                Span::styled(
                    format!("  {}", truncate_width(&spec.description, description_room)),
                    dim(),
                ),
            ])
        })
        .collect();

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(dim());
    frame.render_widget(Paragraph::new(Text::from(lines)).block(block), area);
}

/// Centered modal for the mode picker.
fn draw_picker(frame: &mut Frame, app: &App) {
    let Some(picker) = &app.picker else {
        return;
    };

    let frame_area = frame.area();
    let width = (frame_area.width.saturating_sub(8)).clamp(24, 56);
    let max_rows = frame_area.height.saturating_sub(6).max(1) as usize;
    let height = picker.items.len().min(max_rows) as u16 + 2;
    let area = Rect {
        x: frame_area.x + (frame_area.width.saturating_sub(width)) / 2,
        y: frame_area.y + (frame_area.height.saturating_sub(height)) / 2,
        width,
        height,
    }
    .intersection(frame_area);
    if area.height < 3 || area.width < 4 {
        return;
    }
    frame.render_widget(Clear, area);

    // Window the items so the selection stays visible when the list
    // overflows (selection pinned to the bottom edge while scrolling down).
    let rows = area.height.saturating_sub(2) as usize;
    let start = if picker.selected >= rows {
        picker.selected + 1 - rows
    } else {
        0
    };
    let inner_width = area.width.saturating_sub(2) as usize;

    let lines: Vec<Line<'static>> = picker
        .items
        .iter()
        .enumerate()
        .skip(start)
        .take(rows)
        .map(|(index, item)| {
            let selected = index == picker.selected;
            let marker = if selected { "❯ " } else { "  " };
            let value_style = if selected {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(TEXT_DIM)
            };
            // Ellipsize long model tags so the current marker stays visible.
            let suffix = if item.current { " ●".width() } else { 0 };
            let value_room = inner_width.saturating_sub(2 + suffix + 1);
            let mut spans = vec![
                Span::styled(marker, accent()),
                Span::styled(truncate_width(&item.value, value_room), value_style),
            ];
            if item.current {
                spans.push(Span::styled(" ●", Style::default().fg(Color::White)));
            }
            if !item.detail.is_empty() {
                spans.push(Span::styled(format!("  {}", item.detail), dim()));
            }
            Line::from(spans)
        })
        .collect();

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(dim())
        .title(Line::from(vec![
            Span::styled(" ✦", accent()),
            Span::styled(picker.title.clone(), Style::default().fg(TEXT_DIM)),
        ]))
        .title_bottom(
            Line::from(Span::styled(" ↑↓ move · Enter select · Esc cancel ", dim())).centered(),
        );
    frame.render_widget(Paragraph::new(Text::from(lines)).block(block), area);
}

/// Wrap styled lines at `width` display columns (wide CJK/emoji glyphs
/// count as two) so the transcript can be pinned exactly to its bottom.
/// Wrapping is word-aware: a line breaks at the last space that fits, and
/// only falls back to splitting mid-word when a single word exceeds the
/// content width. Continuation lines keep the hanging indent of their
/// source line (see [`hanging_indent`]), so gutter-indented content stays
/// aligned under its text column. A wide char that no longer fits wraps to
/// the next line first; zero-width chars (combining marks) always stay
/// with their base char.
fn wrap_lines(text: Text<'static>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut out: Vec<Line<'static>> = Vec::new();
    for line in text.lines {
        if line.width() <= width {
            out.push(line);
            continue;
        }
        let indent = hanging_indent(&line).min(width.saturating_sub(1));
        let mut wrapper = LineWrapper::new(width, indent, &line);
        for span in line.spans {
            let style = span.style;
            for ch in span.content.chars() {
                wrapper.feed(ch, style);
            }
        }
        out.append(&mut wrapper.finish());
    }
    out
}

/// Hanging indent (in display columns) for wrapped continuations of `line`:
/// its leading spaces plus one optional short gutter mark — at most two
/// columns of non-alphanumeric glyphs followed by a space, e.g. `❯ `, `· `,
/// `✓ `, `  • `, `▌ `. This is how [`gutter_block`], tool cards, notices,
/// and markdown bullets communicate their text column, so continuation
/// lines stay aligned under it. Lines without such a prefix wrap to
/// column 0.
fn hanging_indent(line: &Line) -> usize {
    let mut chars = line.spans.iter().flat_map(|span| span.content.chars());
    let mut indent = 0usize;
    let mut next = chars.next();
    while let Some(ch) = next {
        if ch != ' ' {
            break;
        }
        indent += 1;
        next = chars.next();
    }
    let mut mark = 0usize;
    while let Some(ch) = next {
        if ch == ' ' {
            // Mark plus its trailing space hang the rest of the message.
            return if mark > 0 { indent + mark + 1 } else { indent };
        }
        if ch.is_alphanumeric() || mark + ch.width().unwrap_or(0) > 2 {
            return indent;
        }
        mark += ch.width().unwrap_or(0);
        next = chars.next();
    }
    indent
}

/// Word-aware wrapping state for one source line. Characters are fed in
/// one at a time (with their span style); words are held back until a
/// space proves they are complete, then committed to the current output
/// line or wrapped whole onto the next. Styles are preserved across
/// breaks by carrying (text, style) runs rather than raw strings.
struct LineWrapper {
    width: usize,
    /// Columns every continuation line starts with (hanging indent).
    indent: usize,
    /// Column the current output line started at: 0 for the first line,
    /// `indent` afterwards.
    start: usize,
    /// Display columns used on the current output line.
    used: usize,
    current: Vec<(String, Style)>,
    /// Word being accumulated, not yet committed to `current`.
    word: Vec<(String, Style)>,
    word_cols: usize,
    /// Spaces seen since the last word, held back so a wrap can eat them.
    spaces: Vec<(String, Style)>,
    space_cols: usize,
    /// Line-level style/alignment of the source line, re-applied to every
    /// wrapped piece.
    line_style: Style,
    alignment: Option<Alignment>,
    done: Vec<Line<'static>>,
}

impl LineWrapper {
    fn new(width: usize, indent: usize, line: &Line<'static>) -> Self {
        Self {
            width,
            indent,
            start: 0,
            used: 0,
            current: Vec::new(),
            word: Vec::new(),
            word_cols: 0,
            spaces: Vec::new(),
            space_cols: 0,
            line_style: line.style,
            alignment: line.alignment,
            done: Vec::new(),
        }
    }

    /// Append one char to a run buffer, merging consecutive equal styles.
    fn push_run(buffer: &mut Vec<(String, Style)>, ch: char, style: Style) {
        match buffer.last_mut() {
            Some((text, last)) if *last == style => text.push(ch),
            _ => buffer.push((ch.to_string(), style)),
        }
    }

    /// Move every run in `from` onto the end of `to`, merging styles.
    fn append_runs(to: &mut Vec<(String, Style)>, from: &mut Vec<(String, Style)>) {
        for (text, style) in from.drain(..) {
            match to.last_mut() {
                Some((last_text, last)) if *last == style => last_text.push_str(&text),
                _ => to.push((text, style)),
            }
        }
    }

    fn feed(&mut self, ch: char, style: Style) {
        if ch == ' ' {
            self.commit_word();
            Self::push_run(&mut self.spaces, ch, style);
            self.space_cols += 1;
            return;
        }
        let ch_width = ch.width().unwrap_or(0);
        if ch_width == 0 && self.word.is_empty() && !self.spaces.is_empty() {
            // A combining mark right after a space stays with that space.
            Self::push_run(&mut self.spaces, ch, style);
            return;
        }
        Self::push_run(&mut self.word, ch, style);
        self.word_cols += ch_width;
    }

    /// Commit the buffered word: onto the current line when it fits after
    /// the held spaces, else onto a fresh continuation line (the break
    /// eats the spaces), hard-splitting only when the word alone exceeds
    /// the content width.
    fn commit_word(&mut self) {
        if self.word.is_empty() {
            return;
        }
        if self.used + self.space_cols + self.word_cols <= self.width {
            self.flush_spaces();
            self.flush_word();
            return;
        }
        if self.used > self.start {
            self.spaces.clear();
            self.space_cols = 0;
            self.newline();
        } else {
            // Line start: keep the source line's leading whitespace.
            self.flush_spaces();
        }
        if self.used + self.word_cols <= self.width {
            self.flush_word();
        } else {
            self.hard_split();
        }
    }

    fn flush_spaces(&mut self) {
        Self::append_runs(&mut self.current, &mut self.spaces);
        self.used += self.space_cols;
        self.space_cols = 0;
    }

    fn flush_word(&mut self) {
        Self::append_runs(&mut self.current, &mut self.word);
        self.used += self.word_cols;
        self.word_cols = 0;
    }

    /// Char-level fallback for a word wider than the content width. A wide
    /// char that no longer fits wraps first; zero-width chars (combining
    /// marks) never split from their base char.
    fn hard_split(&mut self) {
        for (text, style) in std::mem::take(&mut self.word) {
            for ch in text.chars() {
                let ch_width = ch.width().unwrap_or(0);
                if ch_width > 0 && self.used + ch_width > self.width && self.used > self.start {
                    self.newline();
                }
                Self::push_run(&mut self.current, ch, style);
                self.used += ch_width;
            }
        }
        self.word_cols = 0;
    }

    /// Emit the current line and open a continuation at the hanging indent.
    fn newline(&mut self) {
        self.emit();
        if self.indent > 0 {
            self.current
                .push((" ".repeat(self.indent), Style::default()));
        }
        self.used = self.indent;
        self.start = self.indent;
    }

    fn emit(&mut self) {
        let spans: Vec<Span<'static>> = std::mem::take(&mut self.current)
            .into_iter()
            .map(|(text, style)| Span::styled(text, style))
            .collect();
        let mut line = Line::from(spans);
        line.style = self.line_style;
        line.alignment = self.alignment;
        self.done.push(line);
    }

    fn finish(&mut self) -> Vec<Line<'static>> {
        self.commit_word();
        if self.used + self.space_cols <= self.width {
            // Trailing spaces that still fit are kept verbatim.
            self.flush_spaces();
        }
        self.emit();
        std::mem::take(&mut self.done)
    }
}

/// Longest prefix of `text` that fits in `max` display columns (zero-width
/// chars at the boundary stay attached).
fn take_width(text: &str, max: usize) -> &str {
    let mut used = 0usize;
    let mut end = 0usize;
    for (index, ch) in text.char_indices() {
        let ch_width = ch.width().unwrap_or(0);
        if used + ch_width > max {
            break;
        }
        used += ch_width;
        end = index + ch.len_utf8();
    }
    &text[..end]
}

/// Truncate to `max` display columns (not chars), appending `…` when cut.
fn truncate_width(text: &str, max: usize) -> String {
    if text.width() <= max {
        return text.to_string();
    }
    let mut out = take_width(text, max.saturating_sub(1)).to_string();
    out.push('…');
    out
}

// ---------------------------------------------------------------------------
// Syntax highlighting (syntect) — foreground colors only, never backgrounds,
// so the terminal's own background always shows through.
// ---------------------------------------------------------------------------

static SYNTECT_ASSETS: OnceLock<(SyntaxSet, Option<Theme>)> = OnceLock::new();

fn syntect_assets() -> &'static (SyntaxSet, Option<Theme>) {
    SYNTECT_ASSETS.get_or_init(|| {
        let syntaxes = SyntaxSet::load_defaults_newlines();
        let mut themes = ThemeSet::load_defaults();
        let theme = themes.themes.remove("base16-ocean.dark").or_else(|| {
            let key = themes.themes.keys().next().cloned()?;
            themes.themes.remove(&key)
        });
        (syntaxes, theme)
    })
}

/// Map a syntect style to ratatui, collapsing the theme's foreground to its
/// grayscale luminance (the UI is monochrome) and keeping font modifiers —
/// backgrounds would paint over the terminal transparency.
fn syntect_style(style: syntect::highlighting::Style) -> Style {
    let fg = style.foreground;
    let luma = (u32::from(fg.r) * 299 + u32::from(fg.g) * 587 + u32::from(fg.b) * 114) / 1000;
    let luma = luma as u8;
    let mut out = Style::default().fg(Color::Rgb(luma, luma, luma));
    if style.font_style.contains(FontStyle::BOLD) {
        out = out.add_modifier(Modifier::BOLD);
    }
    if style.font_style.contains(FontStyle::ITALIC) {
        out = out.add_modifier(Modifier::ITALIC);
    }
    if style.font_style.contains(FontStyle::UNDERLINE) {
        out = out.add_modifier(Modifier::UNDERLINED);
    }
    out
}

/// Highlight one fenced code block, memoized: completed blocks are
/// re-rendered every frame, so identical (lang, code) pairs hit the cache.
fn highlight_code_block(lang: &str, code: &str) -> Vec<Line<'static>> {
    static CACHE: OnceLock<Mutex<HashMap<u64, Vec<Line<'static>>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));

    let mut hasher = std::hash::DefaultHasher::new();
    lang.hash(&mut hasher);
    code.hash(&mut hasher);
    let key = hasher.finish();
    if let Ok(guard) = cache.lock()
        && let Some(lines) = guard.get(&key)
    {
        return lines.clone();
    }

    let (syntaxes, theme) = syntect_assets();
    let syntax = if lang.is_empty() {
        None
    } else {
        syntaxes.find_syntax_by_token(lang)
    };
    let lines: Vec<Line<'static>> = match (syntax, theme.as_ref()) {
        (Some(syntax), Some(theme)) => {
            let mut highlighter = HighlightLines::new(syntax, theme);
            LinesWithEndings::from(code)
                .map(|line| match highlighter.highlight_line(line, syntaxes) {
                    Ok(ranges) => Line::from(
                        ranges
                            .into_iter()
                            .map(|(style, content)| {
                                Span::styled(
                                    content.trim_end_matches('\n').to_string(),
                                    syntect_style(style),
                                )
                            })
                            .collect::<Vec<_>>(),
                    ),
                    Err(_) => Line::from(Span::styled(
                        line.trim_end_matches('\n').to_string(),
                        Style::default().fg(TEXT_DIM),
                    )),
                })
                .collect()
        }
        _ => code
            .lines()
            .map(|line| {
                Line::from(Span::styled(
                    line.to_string(),
                    Style::default().fg(TEXT_DIM),
                ))
            })
            .collect(),
    };

    if let Ok(mut guard) = cache.lock() {
        if guard.len() >= 128 {
            guard.clear();
        }
        guard.insert(key, lines.clone());
    }
    lines
}

// ---------------------------------------------------------------------------
// Markdown rendering (pulldown-cmark)
// ---------------------------------------------------------------------------

/// Render completed markdown to styled terminal text (fenced code blocks
/// syntax-highlighted, foreground colors only).
pub fn render_markdown(source: &str) -> Text<'static> {
    render_markdown_inner(source, true)
}

/// Render in-flight streaming markdown: identical, except code blocks stay
/// plain so per-frame rendering stays cheap.
fn render_markdown_streaming(source: &str) -> Text<'static> {
    render_markdown_inner(source, false)
}

fn render_markdown_inner(source: &str, highlight: bool) -> Text<'static> {
    let mut renderer = MarkdownRenderer {
        highlight,
        ..MarkdownRenderer::default()
    };
    let options = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    for event in Parser::new_ext(source, options) {
        renderer.event(event);
    }
    renderer.finish()
}

#[derive(Default)]
struct MarkdownRenderer {
    lines: Vec<Line<'static>>,
    current: Vec<Span<'static>>,
    bold: usize,
    italic: usize,
    strike: usize,
    code_block: bool,
    /// Syntax-highlight completed code blocks via syntect.
    highlight: bool,
    /// Fenced language and buffered source of the open code block (only
    /// used when `highlight` is set).
    code_lang: String,
    code_buffer: String,
    heading: bool,
    /// One entry per open list; `Some(n)` carries the next ordered index.
    lists: Vec<Option<u64>>,
    quote_depth: usize,
    link: Option<String>,
}

impl MarkdownRenderer {
    fn style(&self) -> Style {
        let mut style = Style::default();
        if self.code_block {
            // In-flight (unhighlighted) block code: neutral, not loud.
            return style.fg(TEXT_DIM);
        }
        if self.heading {
            style = style.fg(ACCENT).add_modifier(Modifier::BOLD);
        }
        if self.bold > 0 {
            style = style.add_modifier(Modifier::BOLD);
        }
        if self.italic > 0 {
            style = style.add_modifier(Modifier::ITALIC);
        }
        if self.strike > 0 {
            style = style.add_modifier(Modifier::CROSSED_OUT);
        }
        if self.quote_depth > 0 {
            style = style.fg(TEXT_DIM);
        }
        style
    }

    fn end_line(&mut self) {
        self.lines
            .push(Line::from(std::mem::take(&mut self.current)));
    }

    /// Flush the current spans only when non-empty (no spurious blank line).
    fn flush(&mut self) {
        if !self.current.is_empty() {
            self.end_line();
        }
    }

    fn blank_line(&mut self) {
        if !matches!(self.lines.last(), Some(line) if line.spans.is_empty()) {
            self.lines.push(Line::raw(""));
        }
    }

    fn line_prefix(&mut self) {
        if self.quote_depth > 0 {
            self.current
                .push(Span::styled("▌ ".repeat(self.quote_depth), dim()));
        }
        if self.code_block {
            self.current.push(Span::raw("  "));
        }
    }

    fn push_text(&mut self, text: &str) {
        if self.code_block {
            if self.highlight {
                // Buffered for one syntect pass when the block closes.
                self.code_buffer.push_str(text);
                return;
            }
            // Streaming: code blocks carry embedded newlines, plain style.
            let style = self.style();
            let mut first = true;
            for part in text.split('\n') {
                if !first {
                    self.end_line();
                    self.line_prefix();
                }
                first = false;
                if !part.is_empty() {
                    self.current.push(Span::styled(part.to_string(), style));
                }
            }
        } else {
            self.current
                .push(Span::styled(text.to_string(), self.style()));
        }
    }

    fn event(&mut self, event: MdEvent) {
        match event {
            MdEvent::Start(tag) => self.start(tag),
            MdEvent::End(tag) => self.end(tag),
            MdEvent::Text(text) => self.push_text(&text),
            MdEvent::Code(code) => {
                self.current
                    .push(Span::styled(code.to_string(), Style::default().fg(CODE)));
            }
            MdEvent::SoftBreak | MdEvent::HardBreak => {
                self.end_line();
                self.line_prefix();
            }
            MdEvent::Rule => {
                self.flush();
                self.lines
                    .push(Line::from(Span::styled("─".repeat(24), dim())));
                self.blank_line();
            }
            MdEvent::TaskListMarker(checked) => {
                let marker = if checked { "[x] " } else { "[ ] " };
                self.current.push(Span::styled(marker.to_string(), dim()));
            }
            MdEvent::Html(html) | MdEvent::InlineHtml(html) => self.push_text(&html),
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::Paragraph => {
                self.flush();
                self.line_prefix();
            }
            Tag::Heading { .. } => {
                self.flush();
                self.blank_line();
                self.heading = true;
            }
            Tag::BlockQuote(_) => {
                self.flush();
                self.quote_depth += 1;
            }
            Tag::CodeBlock(kind) => {
                self.flush();
                self.code_lang.clear();
                if let CodeBlockKind::Fenced(lang) = kind
                    && !lang.is_empty()
                {
                    self.code_lang.push_str(&lang);
                    self.lines
                        .push(Line::from(Span::styled(format!("  ⌜{lang}⌟"), dim())));
                }
                self.code_block = true;
                self.code_buffer.clear();
                if !self.highlight {
                    self.line_prefix();
                }
            }
            Tag::List(start) => {
                self.flush();
                self.lists.push(start);
            }
            Tag::Item => {
                self.flush();
                let depth = self.lists.len().saturating_sub(1);
                let indent = "  ".repeat(depth);
                let bullet = match self.lists.last_mut() {
                    Some(Some(index)) => {
                        let label = format!("{indent}{index}. ");
                        *index += 1;
                        label
                    }
                    _ => format!("{indent}• "),
                };
                self.current.push(Span::styled(bullet, dim()));
            }
            Tag::Emphasis => self.italic += 1,
            Tag::Strong => self.bold += 1,
            Tag::Strikethrough => self.strike += 1,
            Tag::Link { dest_url, .. } => self.link = Some(dest_url.to_string()),
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                self.flush();
                if self.lists.is_empty() {
                    self.blank_line();
                }
            }
            TagEnd::Heading(_) => {
                self.heading = false;
                self.flush();
            }
            TagEnd::BlockQuote(_) => {
                self.flush();
                self.quote_depth = self.quote_depth.saturating_sub(1);
                self.blank_line();
            }
            TagEnd::CodeBlock => {
                if self.highlight {
                    let code = std::mem::take(&mut self.code_buffer);
                    let lang = std::mem::take(&mut self.code_lang);
                    for mut line in highlight_code_block(&lang, &code) {
                        let mut spans = Vec::with_capacity(line.spans.len() + 1);
                        spans.push(Span::raw("  "));
                        spans.append(&mut line.spans);
                        self.lines.push(Line::from(spans));
                    }
                } else {
                    self.flush();
                }
                self.code_block = false;
                self.blank_line();
            }
            TagEnd::List(_) => {
                self.flush();
                self.lists.pop();
                if self.lists.is_empty() {
                    self.blank_line();
                }
            }
            TagEnd::Item => self.flush(),
            TagEnd::Emphasis => self.italic = self.italic.saturating_sub(1),
            TagEnd::Strong => self.bold = self.bold.saturating_sub(1),
            TagEnd::Strikethrough => self.strike = self.strike.saturating_sub(1),
            TagEnd::Link => {
                if let Some(url) = self.link.take() {
                    self.current
                        .push(Span::styled(format!(" ({url})"), dim().underlined()));
                }
            }
            _ => {}
        }
    }

    fn finish(mut self) -> Text<'static> {
        self.flush();
        while matches!(self.lines.last(), Some(line) if line.spans.is_empty()) {
            self.lines.pop();
        }
        Text::from(self.lines)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Flatten a line's spans into one comparable string.
    fn flat(line: &Line) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn flats(lines: &[Line]) -> Vec<String> {
        lines.iter().map(flat).collect()
    }

    #[test]
    fn wrap_breaks_at_word_boundaries() {
        let lines = wrap_lines(Text::from(Line::raw("the quick brown fox")), 10);
        assert_eq!(flats(&lines), ["the quick", "brown fox"]);
    }

    #[test]
    fn wrap_moves_whole_word_instead_of_splitting() {
        // The recorded defect: "one occurrence" split as "on / e occurrence".
        let lines = wrap_lines(Text::from(Line::raw("one occurrence")), 12);
        assert_eq!(flats(&lines), ["one", "occurrence"]);
    }

    #[test]
    fn wrap_hard_splits_word_longer_than_width() {
        let lines = wrap_lines(Text::from(Line::raw("abcdefghijkl")), 5);
        assert_eq!(flats(&lines), ["abcde", "fghij", "kl"]);
    }

    #[test]
    fn wrap_continuations_keep_hanging_indent() {
        let line = Line::from(vec![
            Span::styled("· ", accent()),
            Span::raw("alpha beta gamma"),
        ]);
        let lines = wrap_lines(Text::from(line), 9);
        assert_eq!(flats(&lines), ["· alpha", "  beta", "  gamma"]);
        // The marker keeps its accent style; continuations stay raw.
        assert_eq!(lines[0].spans[0].style, accent());
    }

    #[test]
    fn wrap_keeps_styles_across_span_boundary_in_one_word() {
        let red = Style::default().fg(Color::Red);
        let blue = Style::default().fg(Color::Blue);
        // "main.py" is one word spanning two styled spans: it must move to
        // the next line whole, with both styles intact.
        let line = Line::from(vec![
            Span::styled("run ma", red),
            Span::styled("in.py", blue),
        ]);
        let lines = wrap_lines(Text::from(line), 7);
        assert_eq!(flats(&lines), ["run", "main.py"]);
        assert_eq!(lines[1].spans[0].content.as_ref(), "ma");
        assert_eq!(lines[1].spans[0].style, red);
        assert_eq!(lines[1].spans[1].content.as_ref(), "in.py");
        assert_eq!(lines[1].spans[1].style, blue);
    }

    #[test]
    fn wrap_wide_chars_never_exceed_width() {
        let lines = wrap_lines(Text::from(Line::raw("日本語のテスト")), 5);
        assert_eq!(flats(&lines), ["日本", "語の", "テス", "ト"]);
        for line in &lines {
            assert!(line.width() <= 5);
        }
    }

    #[test]
    fn wrap_keeps_combining_marks_with_base_char() {
        let lines = wrap_lines(Text::from(Line::raw("e\u{301}".repeat(5))), 3);
        assert_eq!(flats(&lines), ["e\u{301}".repeat(3), "e\u{301}".repeat(2)]);
    }

    #[test]
    fn wrap_leaves_short_lines_untouched() {
        let line = Line::from(vec![Span::styled("❯ ", dim()), Span::raw("hi")]);
        let lines = wrap_lines(Text::from(line.clone()), 10);
        assert_eq!(lines, vec![line]);
    }

    #[test]
    fn hanging_indent_detects_gutter_marks() {
        assert_eq!(hanging_indent(&Line::raw("❯ hello")), 2);
        assert_eq!(hanging_indent(&Line::raw("· hello")), 2);
        assert_eq!(hanging_indent(&Line::raw("✓ tool")), 2);
        assert_eq!(hanging_indent(&Line::raw("  • item")), 4);
        assert_eq!(hanging_indent(&Line::raw("  plain")), 2);
        assert_eq!(hanging_indent(&Line::raw("plain text")), 0);
        // A dim rule is not a mark (wider than two columns of glyphs).
        assert_eq!(hanging_indent(&Line::raw("────────")), 0);
    }
}
