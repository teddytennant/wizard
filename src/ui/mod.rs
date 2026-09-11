//! Ratatui rendering: pure functions from [`App`] state to widgets.
//! Layout: chat transcript (with optional git diff sidebar), optional todo
//! band above the composer, the input line, the subagent rail, and a quiet
//! status line. Floating layers: the command-suggestion popup and the
//! model/mode/rewind/subagent picker.
//!
//! Design rules (do not regress):
//! - **Tokens, never colors**: nothing here names a ratatui `Color`. It asks
//!   [`crate::theme`] for a semantic token (`accent`, `muted`,
//!   `tool.failed`, `diff.add`) and the active theme decides. That is what
//!   makes the low-color fallback, the `ember` theme, and the GUI's visual
//!   continuity one mechanism instead of three.
//! - **Transparent**: never paint a background color; everything renders on
//!   the terminal's own background so it shows through. Selection reads
//!   through an accent marker + bold, not opaque slabs.
//! - **Monochrome by default**: the `minimal` theme is white accent plus dim
//!   grays, with no hues anywhere in chrome or chat. Emphasis reads through
//!   brightness and bold, semantics through glyphs (✓/✗), so nothing is lost
//!   when the palette degrades to 16 colors or to none at all. **Exception:**
//!   the `/diff` sidebar uses conventional green additions / red deletions,
//!   which is the one place a hue carries the meaning. Run status (the
//!   subagent rail, the pane header, the session dashboard) asks for
//!   `tool.done` / `tool.failed` like everything else and lets the theme
//!   decide, so it reads through the glyph plus bold under `minimal` and
//!   through green/red under `ember`.
//! - **No heavy boxes**: borderless sections separated by padding and dim
//!   rules; the theme's border symbols only on floating layers.
//! - **Todos never cover chat**: when shown, the todo list is a reserved
//!   layout band above the composer, so the transcript shrinks instead of
//!   painting under a floating panel.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use pulldown_cmark::{
    Alignment as MdAlignment, CodeBlockKind, Event as MdEvent, Options, Parser, Tag, TagEnd,
};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Constraint, Layout, Margin, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Theme as SyntectTheme, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::agent::ImageSource;
use crate::app::{App, InputMode, PaneStatus, Selection, SubagentPane, TranscriptView};
use crate::config::Mode;
use crate::image_view::{self, ImageBlock, ImageBox, ImageCache};
use crate::images::ImageRef;
use crate::session_registry::SessionState;
use crate::skin::{self, BlockKind, BusyStyle, ComposerFrame, ToolLabel};
use crate::transcript::{ToolItem, TranscriptItem};
// An ordinary import: the theme layer is declared once, as `pub mod theme;` in
// `src/lib.rs`. This line used to be `pub use crate::theme;` as well, a
// re-export that kept the historical `crate::ui::theme` path resolving while
// the module moved out of here; every one of those use sites now spells
// `crate::theme`, so the alias is gone. Declaring the module a second time
// (`#[path = "theme.rs"] pub mod theme;`) would compile and build two unrelated
// copies of every type in it, including two of the process-wide active-theme
// slot, so it must never come back that way.
use crate::theme::{self, Token};
use crate::vim::VimMode;

mod codex;
mod grok;
mod welcome;

use welcome::draw_welcome;

/// The spinner frame for this tick, from the active skin. Every skin turns a
/// different wheel — braille under `wizard`, a five-pointed star under
/// `claude`, a half-circle under `grok` — and they have different frame
/// counts, so the modulus has to come from the table rather than a constant.
pub(super) fn spinner_frame(tick: u64) -> char {
    let frames = skin::chrome().spinner;
    frames[(tick as usize) % frames.len()]
}

/// Tallest the multi-line composer grows before it scrolls internally.
const MAX_INPUT_ROWS: u16 = 10;

/// Column an image block hangs in — the same two-space text column a tool
/// card's output and a notice use, so a tool's image lines up under its card.
const IMAGE_INDENT: u16 = 2;

// Style shorthands for the tokens this module reaches for constantly. Each one
// resolves against the *active* theme on every call, so `/theme` takes effect
// on the next frame with no cached styles to invalidate. Rarer tokens are
// spelled `theme::style(Token::…)` at the call site.

/// Dim chrome: rules, gutter marks, hints, list bullets.
pub(super) fn dim() -> Style {
    theme::style(Token::Faint)
}

/// The single accent used for chrome (prompt, gutters, names, markers).
pub(super) fn accent() -> Style {
    theme::style(Token::Accent)
}

/// Secondary text: tool output, the user's echoed prompt, details.
pub(super) fn muted() -> Style {
    theme::style(Token::Muted)
}

/// Inline code and rendered math. (Block code gets syntect foregrounds, or
/// [`muted`] when it is not highlighted.)
fn code() -> Style {
    theme::style(Token::Code)
}

/// Something the user has to read: a failed tool, an error notice.
fn error() -> Style {
    theme::style(Token::Error)
}

/// Something is off but the turn continues.
pub(super) fn warning() -> Style {
    theme::style(Token::Warning)
}

/// Render one frame. The only entry point the main loop calls.
///
/// Each skin owns its *whole* frame, not just the glyphs inside it: Codex and
/// Grok Build lay the screen out differently enough — where the turn status
/// goes, whether there is a status bar at all, what the composer's border
/// carries — that sharing one layout and swapping markers cannot reach either
/// of them. The shared parts (the transcript blocks, the pickers, the diff
/// sidebar, a subagent pane) are helpers all three call.
pub fn draw(frame: &mut Frame, app: &App) {
    match skin::active() {
        skin::Skin::Codex => codex::draw(frame, app),
        skin::Skin::Grok => grok::draw(frame, app),
        skin::Skin::Wizard => draw_house(frame, app),
    }
}

/// The house frame: transcript, todo band, composer between dim rules, the
/// subagent rail, and a chip-separated status bar.
fn draw_house(frame: &mut Frame, app: &App) {
    let Regions {
        body: main_area,
        status_top,
        todo: todo_area,
        composer: input_area,
        rail: rail_area,
        footer: status_area,
    } = regions(app, frame.area());

    if let Some(pane) = app.attached_pane() {
        // Inside a subagent: its conversation takes over the main area and
        // renders exactly like the main chat.
        draw_pane(frame, app, pane, main_area);
    } else if app.diff.is_some() {
        let [chat_area, side_area] =
            Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)])
                .areas(main_area);
        draw_transcript(frame, app, chat_area);
        draw_diff_sidebar(frame, app, side_area);
    } else {
        draw_transcript(frame, app, main_area);
    }

    // Todos are a reserved band (not a floating overlay), so chat text never
    // paints underneath them. Height is 0 when hidden.
    if todo_area.height > 0 {
        draw_todo_band(frame, app, todo_area);
    }
    draw_input(frame, app, input_area);
    if rail_area.height > 0 {
        draw_rail(frame, app, rail_area);
    }
    // Codex narrates the turn on its own row above the composer; the other
    // two carry it on the status bar below everything.
    if status_top.height > 0 {
        draw_status_above(frame, app, status_top);
    }
    draw_status_bar(
        frame,
        app,
        status_area,
        suggestion_area(app, input_area, frame.area()).is_some(),
    );

    // Floating layers: suggestions stack just above the composer when the
    // user is typing a slash command.
    if !overlay_open(app) {
        draw_suggestions(frame, app, input_area);
    }
    if app.picker.is_some() {
        draw_picker(frame, app);
    }
    if app.plan_review.is_some() {
        draw_plan_review(frame, app);
    }
    if app.interview.is_some() {
        draw_interview(frame, app);
    }
    // The dashboard is modal and full-screen, so it paints last (on top).
    if app.show_dashboard {
        draw_dashboard(frame, app);
    }

    // With any overlay floating above the transcript, a click belongs to the
    // overlay — drop the card hit map so it can't toggle a card underneath.
    if overlay_open(app) {
        app.card_hits.borrow_mut().clear();
    }

    // The selection highlight paints last so it reverses whatever ended up on
    // screen — transcript, sidebar, or an overlay the user dragged across.
    if let Some(selection) = app.selection {
        let area = frame.area();
        let buf = frame.buffer_mut();
        for (y, start, end) in selection_rows(&selection, area.width, area.height) {
            for x in start..end {
                if let Some(cell) = buf.cell_mut(Position::new(x, y)) {
                    cell.modifier.insert(Modifier::REVERSED);
                }
            }
        }
    }
}

/// The rows a frame is laid out into, top to bottom. One place, so what
/// [`draw`] paints and what anything else measures cannot drift apart.
///
/// The shape is a union of what the three skins need, because they do not
/// agree on where a turn's status goes. Wizard and Grok Build put it on a
/// status bar under everything; Codex puts it *above* the composer and leaves
/// only key hints below. A skin that does not use a row gets it at height 0.
#[derive(Debug, Clone, Copy)]
pub struct Regions {
    /// The transcript.
    pub body: Rect,
    /// The turn's status, when the skin puts it above the composer.
    pub status_top: Rect,
    /// The todo band.
    pub todo: Rect,
    /// The composer, frame included.
    pub composer: Rect,
    /// The subagent rail.
    pub rail: Rect,
    /// The bottom row: a status bar, or key hints, depending on the skin.
    pub footer: Rect,
}

/// Lay `area` out for the active skin.
pub fn regions(app: &App, area: Rect) -> Regions {
    // The composer grows with its content (hard line breaks plus soft-wrapped
    // continuations) up to MAX_INPUT_ROWS, then scrolls vertically. +2 for
    // whatever frames it — rules, a border, or the blank rows Codex leaves.
    let budget = composer_budget(area.width);
    let input_rows =
        (wrap_rows(&composer_chars(app), budget).len() as u16).clamp(1, MAX_INPUT_ROWS) + 2;
    // The rail sits between the composer and the footer: one row per subagent,
    // so the dots are always in the same place.
    let rail_rows = rail_height(app);
    // Codex narrates a running turn on its own row directly above the
    // composer, and shows nothing there when idle.
    let status_rows = match skin::chrome().status_above {
        true if app.status.busy || app.rebuilding.is_some() => 1,
        _ => 0,
    };
    let todo_rows = todo_height(app, area.height, input_rows + status_rows, rail_rows);
    let [body, todo, status_top, composer, rail, footer] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(todo_rows),
        Constraint::Length(status_rows),
        Constraint::Length(input_rows),
        Constraint::Length(rail_rows),
        Constraint::Length(1),
    ])
    .areas(area);
    Regions {
        body,
        status_top,
        todo,
        composer,
        rail,
        footer,
    }
}

/// Rows reserved for the todo band (0 when hidden). Caps so a long list
/// cannot swallow the transcript; leaves room for the composer, rail, status
/// bar, and at least one row of chat.
fn todo_height(app: &App, total_height: u16, input_rows: u16, rail_rows: u16) -> u16 {
    if !app.show_todos {
        return 0;
    }
    let item_rows = if app.todos.is_empty() {
        1u16
    } else {
        app.todos.len() as u16
    };
    // +2 for the rounded border. Floor of 3 keeps the title + one item row.
    let desired = (item_rows + 2).max(3);
    // Keep at least 1 transcript row + composer + rail + status above the
    // hard floor so chat never disappears under the band.
    let reserved = 1u16
        .saturating_add(input_rows)
        .saturating_add(rail_rows)
        .saturating_add(1);
    let available = total_height.saturating_sub(reserved);
    if available < 3 {
        return 0;
    }
    desired.min(available).min(12)
}

/// Per-row spans `(y, start_x, end_x_exclusive)` a selection covers over a grid
/// `width` × `height`. Reading-order flow: the first row runs from the start
/// column to the edge, full rows in between, the last row from the edge to the
/// head column (inclusive). Shared by the highlight overlay and the clipboard
/// extraction so what's shown is exactly what's copied.
pub(super) fn selection_rows(
    selection: &Selection,
    width: u16,
    height: u16,
) -> Vec<(u16, u16, u16)> {
    let ((start_x, start_y), (end_x, end_y)) = selection.ordered();
    let mut rows = Vec::new();
    let last_y = end_y.min(height.saturating_sub(1));
    for y in start_y..=last_y {
        let row_start = if y == start_y { start_x } else { 0 }.min(width);
        // Include the cell under the head: end column + 1, clamped to the edge.
        let row_end = if y == end_y {
            end_x.saturating_add(1)
        } else {
            width
        }
        .min(width);
        if row_start < row_end {
            rows.push((y, row_start, row_end));
        }
    }
    rows
}

/// Extract the text under a selection from a rendered cell buffer, in reading
/// order, one `\n` per screen row. Trailing whitespace is trimmed per line so
/// the copy isn't padded out to the row width. Shared leading whitespace
/// across the selected rows is then stripped, so a drag that covers the
/// transcript gutter (the one-column side margin, the `· `/`❯ ` marker, the
/// grok rail) does not paste as an indented block.
pub fn selection_text(buf: &Buffer, selection: &Selection) -> String {
    let area = buf.area;
    let rows = selection_rows(selection, area.width, area.height);
    // Stream selection starts the first row at the click and every row
    // below it at column 0, so the highlight covers the gutter on
    // continuation rows. Those columns were never under the cursor on
    // the first row; remember how many we skipped so the indent strip
    // can treat them as leading spaces the first line "has".
    let first_omitted = rows.first().map(|&(_, start, _)| start).unwrap_or(0);
    let mut lines: Vec<String> = Vec::with_capacity(rows.len());
    for (y, start, end) in rows {
        let mut line = String::new();
        for x in start..end {
            if let Some(cell) = buf.cell(Position::new(x, y)) {
                line.push_str(cell.symbol());
            }
        }
        lines.push(line.trim_end().to_string());
    }
    strip_shared_leading_indent(&mut lines, first_omitted);
    let out = lines.join("\n");
    // A selection of only blank cells trims to nothing; report it as empty so
    // the caller skips the copy.
    if out.trim().is_empty() {
        String::new()
    } else {
        out
    }
}

/// Drop the run of leading spaces every non-blank line shares.
///
/// A drag across a Wizard transcript almost always includes the gutter: the
/// body is inset one column from the terminal edge, assistant rows start
/// `· `, user rows `❯ `, grok rows a rail plus two pads. Those columns are
/// chrome. Relative indent inside the selection (code fences, nested lists)
/// is left alone because it is not shared.
///
/// `first_omitted` is the start column of the first selected row. Stream
/// selection skips those cells on the first row and includes them on every
/// row below, which is how a copy used to grow a left margin after line one.
fn strip_shared_leading_indent(lines: &mut [String], first_omitted: u16) {
    // A one-line drag of indented code has nothing to compare against, so the
    // indent is content. The gutter only becomes obvious across several rows.
    let leading = |line: &str| line.chars().take_while(|c| *c == ' ').count();
    let mut min = None;
    let mut count = 0usize;
    for (index, line) in lines.iter().enumerate() {
        if line.is_empty() {
            continue;
        }
        let n = leading(line)
            + if index == 0 {
                first_omitted as usize
            } else {
                0
            };
        count += 1;
        min = Some(min.map_or(n, |m: usize| m.min(n)));
    }
    if count < 2 {
        return;
    }
    let Some(indent) = min.filter(|&n| n > 0) else {
        return;
    };
    for (index, line) in lines.iter_mut().enumerate() {
        if line.is_empty() {
            continue;
        }
        let skip = if index == 0 {
            indent.saturating_sub(first_omitted as usize)
        } else {
            indent
        };
        *line = line.chars().skip(skip).collect();
    }
}

/// Chat transcript: user/assistant messages with streaming markdown and
/// collapsible tool cards. Borderless; a one-column side margin keeps the
/// text off the terminal edge. Shows the welcome screen while empty.
pub(super) fn draw_transcript(frame: &mut Frame, app: &App, area: Rect) {
    // Rebuilt from scratch every frame; cleared up front so the early
    // returns below can't leave stale clickable rows behind.
    app.card_hits.borrow_mut().clear();

    // Stay on the welcome screen until the conversation actually begins (see
    // `App::welcome_visible`: early system notices alone don't dismiss it,
    // but any submission — even a slash command — does).
    if app.welcome_visible() {
        // A slash-command menu (e.g. `/provider`) or other modal floats over a
        // small centered area; the welcome card would show through around it.
        // Drop the card while any overlay is open so there's no text overlay.
        if !overlay_open(app) {
            draw_welcome(frame, app, area);
        }
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

    let mut cache = app.images.borrow_mut();
    // Each block wraps its own content to what its chrome leaves it and comes
    // back already decorated, so nothing here re-wraps: a second pass would cut
    // an accent column off the row it belongs to, and a tinted row is exactly
    // `inner_width` wide by construction.
    let rendered = transcript_text(app, &mut cache, image_box(inner), inner_width);
    let (lines, row_tags) = (rendered.lines, rendered.tags);
    let total = lines.len();
    let max_scroll = total.saturating_sub(inner_height);
    // Cache for key handlers so they can convert a follow-tail view into a
    // stable top-anchored offset without re-wrapping the transcript.
    app.transcript.max_scroll.set(max_scroll as u16);
    // Stick-to-bottom: when following (or the content still fits), pin to the
    // live tail. Otherwise hold the absolute top-of-viewport offset so new
    // streaming lines do not yank the user away from what they were reading.
    let start = if app.transcript.follow || max_scroll == 0 {
        max_scroll
    } else {
        (app.transcript.scroll as usize).min(max_scroll)
    };
    let remaining = max_scroll.saturating_sub(start);
    let end = (start + inner_height).min(total);
    let visible: Vec<Line<'static>> = lines[start..end].to_vec();

    // Record where card headers landed on screen for click-to-toggle.
    {
        let mut hits = app.card_hits.borrow_mut();
        for (offset, tag) in row_tags[start..end].iter().enumerate() {
            if let RowTag::Card(index) = tag {
                hits.push((inner.y + offset as u16, *index));
            }
        }
    }

    // Before the move: the scroll hint below needs to know whether the top row
    // has room for it.
    let first_row_width = visible.first().map(|line| line.width() as u16).unwrap_or(0);

    frame.render_widget(Paragraph::new(Text::from(visible)), inner);

    // Then the pixels, into the rows the text left blank for them. Skipped
    // under an overlay: the modal owns the screen, and an image drawn with a
    // terminal graphics protocol must not paint through it.
    if !overlay_open(app) {
        paint_images(
            frame,
            inner,
            &row_tags[start..end],
            &rendered.blocks,
            &mut cache,
        );
    }

    // Scrolled away from the tail: a quiet hint in the top-right corner —
    // when the top row has room for it.
    //
    // It used to `Clear` its rect and draw regardless, so on a full first line
    // it ate the words underneath: a `/doctor` row became
    // "✓ system prompt  4 section(s), 3.0 K↓ 10 more" with "iB, ~760 t" simply
    // gone, and `/help` lost "plan is" out of the middle of a sentence. Silent,
    // and indistinguishable from the text being wrong.
    //
    // A hint is decoration; the transcript is the thing the user came for. When
    // they compete the hint yields, and it can afford to: the scrollbar drawn
    // just below says the same thing in the margin, where nothing is written.
    if remaining > 0 {
        let label = format!("↓ {remaining} more ");
        let width = (label.width() as u16).min(inner.width);
        if first_row_width + width <= inner.width {
            let hint = Rect {
                x: inner.right().saturating_sub(width),
                y: inner.y,
                width,
                height: 1,
            };
            frame.render_widget(Clear, hint);
            frame.render_widget(Paragraph::new(Span::styled(label, dim())), hint);
        }
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

/// Colored span for a mode name: genie is quiet, sovereign is a warning.
pub(super) fn mode_span(mode: Mode) -> Span<'static> {
    match mode {
        Mode::Genie => Span::styled("genie", muted()),
        Mode::Sovereign => Span::styled("sovereign", warning().bold()),
    }
}

/// The status-bar model label. Loud (accent, bold) while `/fusion` or `/ultra`
/// is on, dim otherwise. Both cost several× the tokens of a plain turn — fusion
/// runs every turn through a panel of models, ultra fans the active one out over
/// N drafting candidates — and both are sticky, so neither is left running
/// unnoticed. Ultra also gets its own `ULTRA ×N` chip in
/// [`draw_status_bar`](self::draw_status_bar), because unlike fusion it does not
/// change the model string this label renders.
pub(super) fn model_span(app: &App) -> Span<'static> {
    if app.fusion_active || app.ultra.is_some() {
        Span::styled(app.status.model.clone(), accent().bold())
    } else {
        Span::styled(app.status.model.clone(), muted())
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

/// What a rendered transcript row belongs to. Rows are wrapped and sliced by
/// the scroll before anything is painted, so this is how a row on screen is
/// traced back to what put it there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RowTag {
    /// Ordinary text.
    Text,
    /// The *header* line of the tool card at this transcript index — the
    /// click-to-toggle target.
    Card(usize),
    /// Row `row` of the image block at `slot` in [`Rendered::blocks`]. The row
    /// is left blank in the text and the pixels are painted into it afterwards
    /// (see [`paint_images`]), so an image scrolls and clips like any other
    /// content.
    Image { slot: usize, row: u16 },
}

/// A transcript rendered to lines: what each row is, and the image blocks whose
/// rows are waiting to be painted.
pub(super) struct Rendered {
    lines: Vec<Line<'static>>,
    tags: Vec<RowTag>,
    blocks: Vec<ImageBlock>,
    /// Nothing has been pushed yet, so a caller appending more gets the
    /// blank-line spacing right.
    empty: bool,
}

/// The cells an image may take in a transcript body: the text column it hangs
/// in, capped so it stays a thumbnail, and never more than half the viewport —
/// one image cannot push the conversation off the screen.
pub(super) fn image_box(inner: Rect) -> ImageBox {
    ImageBox {
        cols: inner
            .width
            .saturating_sub(IMAGE_INDENT)
            .min(image_view::MAX_COLS),
        rows: (inner.height / 2).clamp(1, image_view::MAX_ROWS),
    }
}

/// Is a floating layer covering the transcript? Then a click belongs to the
/// overlay rather than a card underneath, and nothing of the transcript's own —
/// its images least of all — should paint through.
pub(super) fn overlay_open(app: &App) -> bool {
    app.picker.is_some()
        || app.plan_review.is_some()
        || app.interview.is_some()
        || app.show_dashboard
}

/// The two lines that always accompany an image, whatever the terminal can
/// draw: what made it and how big it is, then the file itself. The path gets a
/// line to itself so it survives a drag-select as one string — it is how the
/// user opens the real thing.
fn image_caption(source: &ImageSource, image: &ImageRef) -> Vec<Line<'static>> {
    let from = match source.tool() {
        Some(tool) => format!("image from `{tool}`"),
        None => "image".to_string(),
    };
    // The caption hangs off the same marker the message that produced the
    // image does, so it lines up under whatever said it. Under the skins whose
    // marker is blanks this is the two-column indent it always was.
    let marker = skin::chrome().blocks.of(BlockKind::Assistant).marker;
    let mark = move || Span::styled(marker.rest, marker.style());
    vec![
        Line::from(vec![
            mark(),
            Span::styled(
                format!(
                    "{from} · {} · {} KB",
                    image.mime,
                    image.bytes.div_ceil(1024)
                ),
                dim().italic(),
            ),
        ]),
        Line::from(vec![
            mark(),
            Span::styled(image.path.display().to_string(), muted()),
        ]),
    ]
}

/// Build the full (unwrapped) transcript text from app state, plus the per-row
/// tags and the image blocks the rows were reserved for.
///
/// `content_width` is the display columns available for markdown body text
/// (terminal inner width minus the two-column gutter). Tables use it so their
/// columns shrink and cells wrap instead of the whole grid soft-wrapping.
pub(super) fn transcript_text(
    app: &App,
    cache: &mut ImageCache,
    budget: ImageBox,
    width: usize,
) -> Rendered {
    let Rendered {
        mut lines,
        mut tags,
        blocks,
        empty,
    } = items_text(&app.transcript, app.tick, cache, budget, width);
    let mut first = empty;
    // The uncommitted tail: what the model is saying right now, below
    // everything it has finished saying. Decorated exactly like a committed
    // block, so nothing shifts sideways at the moment a turn lands.
    let (thinking, streaming) = app.transcript.streaming();
    let chrome = skin::chrome();
    let decorate = |lines: &mut Vec<Line<'static>>, kind: BlockKind, text: Text<'static>| {
        let style = chrome.blocks.of(kind);
        let content = wrap_lines(text, style.content_width(width as u16) as usize);
        lines.extend(skin::layout::decorate(
            style,
            content,
            width as u16,
            app.tick,
            true,
        ));
    };

    if !thinking.is_empty() {
        if !first {
            lines.push(Line::raw(""));
        }
        first = false;
        // In-flight reasoning, dimmed so it reads as background noise.
        decorate(&mut lines, BlockKind::Thinking, thinking_text(thinking));
    }
    if !streaming.is_empty() {
        if !first {
            lines.push(Line::raw(""));
        }
        // Streaming: the text itself arriving, with a soft cursor at the
        // tail. Code blocks stay unhighlighted while in flight (cheap to
        // re-render every frame).
        let content_width = chrome
            .blocks
            .of(BlockKind::Assistant)
            .content_width(width as u16) as usize;
        let mut text = render_markdown_streaming(streaming, content_width);
        let tail = Span::styled("▍", dim());
        match text.lines.last_mut() {
            Some(last) => last.spans.push(tail),
            None => text.lines.push(Line::from(tail)),
        }
        decorate(&mut lines, BlockKind::Assistant, text);
    } else if app.status.busy && !tool_running(&app.transcript) {
        // Waiting on the model with nothing to show for it yet. A running
        // tool's card is its own indicator, so this row stays away then.
        if !first {
            lines.push(Line::raw(""));
        }
        lines.push(Line::from(busy_row(app)));
    }

    tags.resize(lines.len(), RowTag::Text);
    Rendered {
        lines,
        tags,
        blocks,
        empty: first,
    }
}

/// Whether the newest tool row is still waiting on its result.
fn tool_running(view: &TranscriptView) -> bool {
    matches!(view.last(), Some(TranscriptItem::Tool(tool)) if tool.output.is_none())
}

/// The row shown while the model has been asked and has not answered: the
/// spinner and how long it has been. A custom `[ui] spinner_verbs` list still
/// puts its word in front; the stock look has none.
pub(super) fn busy_row(app: &App) -> Vec<Span<'static>> {
    let elapsed = app
        .turn_started
        .map(|started| started.elapsed())
        .unwrap_or_default();
    let mut spans = vec![Span::styled(
        format!("{} ", spinner_frame(app.tick)),
        accent(),
    )];
    if !app.config.ui.spinner_verbs.is_empty() {
        spans.push(Span::styled(
            format!("{}… ", app.spinner_verb),
            dim().italic(),
        ));
    }
    spans.push(Span::styled(fmt_elapsed(elapsed), dim()));
    // The round trips so far, once there is one; the budget too when the
    // turn has one.
    let step = match (app.status.step, app.status.max_steps.cap()) {
        (0, None) => None,
        (step, None) => Some(format!(" · step {step}")),
        (step, Some(cap)) => Some(format!(" · step {step}/{cap}")),
    };
    if let Some(step) = step {
        spans.push(Span::styled(step, dim()));
    }
    spans
}

/// Render a conversation to lines, tagging each row with what it belongs to
/// and reserving the rows an image block will be painted into.
///
/// The rows come straight off [`crate::transcript::TranscriptItem`]s — there
/// is no intermediate per-surface entry type any more, so a live turn and its
/// replay cannot be drawn from two different readings of the same events.
/// What the *view* contributes is one thing: which tool rows are folded
/// ([`TranscriptView::folded`]).
///
/// Shared by the main transcript and by a subagent's pane, which is what makes
/// an attached pane render identically to the main chat — images included.
/// `content_width` is the body column tables may fill (see [`transcript_text`]).
pub(super) fn items_text(
    view: &TranscriptView,
    tick: u64,
    cache: &mut ImageCache,
    budget: ImageBox,
    width: usize,
) -> Rendered {
    let chrome = skin::chrome();
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut tags: Vec<RowTag> = Vec::new();
    let mut blocks: Vec<ImageBlock> = Vec::new();
    let mut prev_tool = false;
    let mut prev_notice = false;
    let mut first = true;

    for (index, item) in view.iter().enumerate() {
        // A turn boundary has no row of its own: the TUI shows a continuous
        // conversation, and `/rewind` is what turns are for.
        if matches!(item, TranscriptItem::TurnMarker { .. }) {
            continue;
        }
        let is_tool = matches!(item, TranscriptItem::Tool(_));
        let is_notice = matches!(item, TranscriptItem::Notice(_));
        let is_image = matches!(item, TranscriptItem::Images { .. });
        // Comfortable spacing between turns; runs of tool cards or notices
        // stay tight so they read as one group. An image is always tight: it
        // belongs to whatever produced it, so it hangs directly under that
        // message or card rather than floating between them.
        let tight = (is_tool && prev_tool) || (is_notice && prev_notice) || is_image;
        if !first && !tight {
            lines.push(Line::raw(""));
        }
        first = false;
        prev_tool = is_tool;
        prev_notice = is_notice;

        // A tool card's first pushed line is its header (glyph + name).
        let header_at = lines.len();
        // Where the first image block's rows begin, once it has any.
        let mut image_rows: Vec<(usize, usize, u16)> = Vec::new();

        // Which block this is, and therefore how it is decorated. The content
        // below is built to fit *inside* that decoration and wrapped to it,
        // because the marker and the accent column go on afterwards — see
        // [`crate::skin::layout`] for why that order is the whole point.
        let kind = match item {
            TranscriptItem::User { .. } => BlockKind::User,
            TranscriptItem::Thinking(_) => BlockKind::Thinking,
            TranscriptItem::Tool(_) => BlockKind::Tool,
            TranscriptItem::Notice(_) => BlockKind::Notice,
            _ => BlockKind::Assistant,
        };
        let style = chrome.blocks.of(kind);
        let content_width = style.content_width(width as u16) as usize;
        let mut running = false;

        let content: Vec<Line<'static>> = match item {
            TranscriptItem::TurnMarker { .. } => Vec::new(),
            // The user's own attachments have no row, which is what the live
            // path does with them too (see `App::record_prompt`): drawing one
            // on replay would make a resumed conversation look unlike the one
            // it resumed.
            TranscriptItem::User { text, .. } => wrap_all(
                text.lines()
                    .map(|line| Line::from(Span::styled(line.to_string(), muted())))
                    .collect(),
                content_width,
            ),
            TranscriptItem::Text(message) => {
                wrap_lines(render_markdown_at(message, content_width), content_width)
            }
            TranscriptItem::Thinking(message) => wrap_lines(thinking_text(message), content_width),
            TranscriptItem::Tool(tool) => {
                running = tool.output.is_none();
                tool_card_lines(tool, view.folded(index), tick, content_width)
            }
            // Images are the one entry that is not decorated: its rows are
            // reserved for pixels painted straight into the cells afterwards,
            // and a slab or a marker in those columns would be overwritten
            // anyway. The caption below them carries the block's marker.
            TranscriptItem::Images { source, images } => {
                for image in images {
                    // Reserve the block's rows as blank lines. They wrap to
                    // themselves, scroll with the text, and are painted last —
                    // which is what keeps the pixels inside their own rows
                    // however the transcript moves. A terminal that can draw
                    // nothing reserves nothing, and the caption below stands
                    // alone.
                    if let Some(block) = cache.layout(image, budget) {
                        image_rows.push((lines.len(), blocks.len(), block.rows));
                        lines.extend(std::iter::repeat_n(Line::raw(""), block.rows as usize));
                        blocks.push(block);
                    }
                    lines.extend(image_caption(source, image));
                }
                Vec::new()
            }
            TranscriptItem::Notice(message) => {
                // An error leads with the same glyph a failed tool gets, so
                // it reads as one under every color depth; the word is gone
                // because the glyph is the word.
                let (style, message) = match message.strip_prefix("error: ") {
                    Some(rest) => (error().bold(), format!("✗ {rest}")),
                    None if message.starts_with("error") => (error().bold(), message.clone()),
                    None => (dim().italic(), message.clone()),
                };
                wrap_all(
                    message
                        .lines()
                        .map(|line| Line::from(Span::styled(line.to_string(), style)))
                        .collect(),
                    content_width,
                )
            }
        };
        if !content.is_empty() {
            lines.extend(skin::layout::decorate(
                style,
                content,
                width as u16,
                tick,
                running,
            ));
        }

        // Keep the tags in lockstep with whatever the item pushed: a tool
        // card's header line is clickable, an image block's rows are paintable,
        // everything else is text.
        //
        // The header is the block's first *content* row, so it sits below
        // whatever vertical padding the skin asked for — click the slab's
        // margin and nothing should fold.
        tags.resize(lines.len(), RowTag::Text);
        let header_at = header_at + style.pad_y as usize;
        if is_tool && header_at < lines.len() {
            tags[header_at] = RowTag::Card(index);
        }
        for (at, slot, rows) in image_rows {
            for row in 0..rows {
                tags[at + row as usize] = RowTag::Image { slot, row };
            }
        }
    }

    Rendered {
        lines,
        tags,
        blocks,
        empty: first,
    }
}

/// Human label + one-line summary for a tool call. `spawn_subagent` reads as
/// "subagent <name> · <task>" so the user can see which subagent is working
/// and on what; every other tool is its own name plus its JSON args. The
/// summary is returned untruncated — callers clip it to their width.
///
/// `grammar` is the active skin's ([`ToolLabel`]), and it moves the argument
/// summary around rather than changing it:
///
/// - `Plain` — `bash` + `{"command":"ls"}`, the house form.
/// - `Ran` — `Ran ls`, Codex's: a past-tense verb in the label, arguments
///   still in the summary.
///
/// All three prefer a *readable* argument to the JSON blob when the call has
/// one obvious subject — a command, a path, a pattern. `{"command":"ls -la"}`
/// says nothing `ls -la` does not, and it costs three quarters of the width to
/// say it.
fn tool_label(name: &str, args: &serde_json::Value, grammar: ToolLabel) -> (String, String) {
    if name == "spawn_subagent" {
        let who = args.get("subagent").and_then(|v| v.as_str()).unwrap_or("?");
        let task = args.get("task").and_then(|v| v.as_str()).unwrap_or("");
        let summary = if task.is_empty() {
            who.to_string()
        } else {
            format!("{who} · {task}")
        };
        return (label_for("subagent", grammar), summary);
    }
    let summary = if args.is_null() {
        String::new()
    } else {
        match args.get("command").or_else(|| args.get("path")) {
            Some(serde_json::Value::String(subject)) => subject.clone(),
            _ => serde_json::to_string(args).unwrap_or_default(),
        }
    };
    (label_for(name, grammar), summary)
}

/// The label half of [`tool_label`].
fn label_for(base: &str, grammar: ToolLabel) -> String {
    match grammar {
        ToolLabel::Plain => base.to_string(),
        ToolLabel::Ran => format!("Ran {base}"),
    }
}

/// Render one tool invocation as a compact single-line card: status glyph,
/// tool name in accent, truncated args in dim. Output expands below only
/// when relevant (short successful outputs, or Ctrl-T).
///
/// `running` is the call not having landed yet, which is *not* the same as
/// `output` being `None`: a foreground command streams its output into the card
/// while it runs (see [`crate::transcript::ToolItem::progress`]), so a card can
/// have a body and still be waiting.
/// Returns the card's *content* rows, already wrapped to `width` — the block's
/// own decoration (accent column, pads, slab) goes on afterwards, in
/// [`crate::skin::layout::decorate`].
fn tool_card_lines(
    tool: &ToolItem,
    collapsed: bool,
    tick: u64,
    width: usize,
) -> Vec<Line<'static>> {
    const MAX_OUTPUT_LINES: usize = 200;
    let mut lines: Vec<Line<'static>> = Vec::new();

    let (name, args) = (tool.name.as_str(), &tool.args);
    let result = tool.output.as_ref();
    // A call with no result yet is still running — but it may already have a
    // body, because a foreground command streams its output into the card while
    // it works (see [`crate::transcript::ToolItem::progress`]). The spinner
    // keeps turning either way: a ✓ over a half-written prompt would be a lie.
    let running = result.is_none();
    let is_error = result.is_some_and(|result| result.is_error);
    let output = match result {
        Some(result) => Some(result.content.as_str()),
        None if !tool.progress.is_empty() => Some(tool.progress.as_str()),
        None => None,
    };
    // `execute` folds a non-zero exit into its last output line; the header
    // is where the reader looks for it, so it moves up there and the body
    // keeps only what the command printed.
    let (output, exit_code) = match output {
        Some(text) if is_error => split_exit_code(text),
        other => (other, None),
    };

    let chrome = skin::chrome();
    let glyph = match (running, is_error) {
        (true, _) => Span::styled(
            spinner_frame(tick).to_string(),
            theme::style(Token::ToolRunning),
        ),
        (false, false) => Span::styled(chrome.tool_done, theme::style(Token::ToolDone)),
        (false, true) => Span::styled(chrome.tool_failed, theme::style(Token::ToolFailed).bold()),
    };

    let (label, mut summary) = tool_label(name, args, chrome.tool_label);
    // An edit names its file once, here, with the line it landed on; the body
    // below is the change itself rather than a sentence about it.
    let edit = edit_diff_lines(name, args, output.filter(|_| !is_error));
    if edit.is_some()
        && let Some(line) = edited_line(output.unwrap_or(""))
    {
        summary = format!("{summary}:{line}");
    }
    let mut card = vec![glyph, Span::raw(" "), Span::styled(label, accent())];
    if !summary.is_empty() {
        // `Call` has already folded the arguments into the label's parentheses;
        // pushing them again would print them twice.
        card.push(Span::styled(
            format!("  {}", truncate_width(&summary, 64)),
            dim(),
        ));
    }
    if let Some(code) = exit_code {
        card.push(Span::styled(format!("  exit {code}"), muted()));
    }
    if let Some(took) = tool.timing.elapsed() {
        card.push(Span::styled(format!("  {}", fmt_elapsed(took)), dim()));
    }
    let hidden = match &edit {
        Some(rows) => rows.len(),
        None => output.map(|text| text.lines().count()).unwrap_or(0),
    };
    if collapsed && hidden > 0 {
        card.push(Span::styled(format!("  +{hidden} lines"), dim().italic()));
    }
    // The header is one row by construction: it is a summary, and a summary
    // that wraps onto a second line has stopped being one.
    lines.push(truncate_line(Line::from(card), width));

    let (first_arm, rest_arm) = chrome.tool_output;
    // The arm is drawn once, on the first body row, and replaced by blanks
    // of the same width below it — that is what makes Claude Code's `⎿`
    // and Codex's `└` read as one arm rather than a column of them. The
    // body is wrapped to what is left *after* the arm and prefixed
    // afterwards, so a long output line keeps the indent when it wraps.
    let body_width = width.saturating_sub(first_arm.width()).max(1);
    if collapsed {
        return lines;
    }
    if let Some(rows) = edit {
        lines.extend(prefix_rows(
            wrap_all(rows, body_width),
            first_arm,
            rest_arm,
            dim(),
        ));
        return lines;
    }
    if let Some(text) = output {
        let out_lines: Vec<&str> = text.lines().collect();
        let over = out_lines.len().saturating_sub(MAX_OUTPUT_LINES);
        // A finished result is read from the top; a running command is read
        // from the bottom, because the line it is waiting on is the last one.
        let shown: &[&str] = if running {
            &out_lines[over..]
        } else {
            &out_lines[..out_lines.len() - over]
        };

        let mut body: Vec<Line<'static>> = Vec::new();
        if over > 0 && running {
            body.push(Line::from(Span::styled(
                format!("… {over} earlier lines"),
                dim(),
            )));
        }
        body.extend(
            shown
                .iter()
                .map(|line| Line::from(Span::styled((*line).to_string(), muted()))),
        );
        if over > 0 && !running {
            // Codex names the key that reveals the rest; Wizard's is Ctrl-T,
            // which toggles the last tool card, so the hint names that one.
            let hint = match chrome.tool_label {
                ToolLabel::Ran => format!("… +{over} lines (ctrl + t to expand)"),
                _ => format!("… +{over} lines"),
            };
            body.push(Line::from(Span::styled(hint, dim())));
        }
        lines.extend(prefix_rows(
            wrap_all(body, body_width),
            first_arm,
            rest_arm,
            dim(),
        ));
    }
    lines
}

/// Split the `exit code: N` line [`crate::tools::shell::render_command_result`]
/// appends off a failed command's output. The body that comes back may be
/// empty, which is a command that failed silently, and that is worth showing
/// as exactly that.
fn split_exit_code(text: &str) -> (Option<&str>, Option<i32>) {
    let (body, last) = text.rsplit_once('\n').unwrap_or(("", text));
    match last
        .strip_prefix("exit code: ")
        .and_then(|n| n.trim().parse().ok())
    {
        Some(code) => (Some(body).filter(|body| !body.is_empty()), Some(code)),
        None => (Some(text), None),
    }
}

/// The line number an `edit_file` confirmation names (`… (line 12)`).
fn edited_line(output: &str) -> Option<u32> {
    let (_, rest) = output.split_once("(line ")?;
    rest.split(')').next()?.trim().parse().ok()
}

/// An edit's body as the change itself: the removed lines with `-`, the added
/// lines with `+`, in the diff colors, from the arguments the model sent.
/// `write_file` is all additions. `None` for any other tool, or for an edit
/// that failed, whose output is the error and stays as text.
fn edit_diff_lines(
    name: &str,
    args: &serde_json::Value,
    output: Option<&str>,
) -> Option<Vec<Line<'static>>> {
    let arg = |key: &str| args.get(key).and_then(serde_json::Value::as_str);
    let (old, new) = match name {
        "edit_file" => (arg("old_string")?, arg("new_string")?),
        "write_file" => ("", arg("content")?),
        _ => return None,
    };
    output?;
    let mut rows = Vec::new();
    for line in old.lines() {
        rows.push(Line::from(Span::styled(
            format!("- {line}"),
            theme::style(Token::DiffDel),
        )));
    }
    for line in new.lines() {
        rows.push(Line::from(Span::styled(
            format!("+ {line}"),
            theme::style(Token::DiffAdd),
        )));
    }
    Some(rows)
}

/// `0.4s`, `12s`, `2m05s`: as much precision as the number has.
pub(super) fn fmt_elapsed(took: std::time::Duration) -> String {
    let secs = took.as_secs_f64();
    if secs < 10.0 {
        format!("{secs:.1}s")
    } else if secs < 60.0 {
        format!("{}s", secs as u64)
    } else {
        let whole = secs as u64;
        format!("{}m{:02}s", whole / 60, whole % 60)
    }
}

/// Wrap every line to `width`, keeping them in order. The rows that come out
/// all fit, which is what lets [`crate::skin::layout::decorate`] put chrome in
/// front of each one without anything reflowing afterwards.
pub(super) fn wrap_all(lines: Vec<Line<'static>>, width: usize) -> Vec<Line<'static>> {
    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        out.append(&mut wrap_lines(Text::from(vec![line]), width));
    }
    out
}

/// Prefix already-wrapped rows: `head` on the first, `rest` on the others.
///
/// The Rust of `prefix_lines` from `codex-rs/tui/src/render/line_utils.rs`
/// (openai/codex, Apache-2.0), which is where the wrap-then-prefix order comes
/// from. See `docs/ui-skins.md`.
pub(super) fn prefix_rows(
    rows: Vec<Line<'static>>,
    head: &'static str,
    rest: &'static str,
    style: Style,
) -> Vec<Line<'static>> {
    rows.into_iter()
        .enumerate()
        .map(|(index, line)| {
            let mark = if index == 0 { head } else { rest };
            let mut spans = Vec::with_capacity(line.spans.len() + 1);
            if !mark.is_empty() {
                spans.push(Span::styled(mark, style));
            }
            spans.extend(line.spans);
            Line::from(spans)
        })
        .collect()
}

/// Compact todo band just above the composer (`/todos`, auto-shown on the
/// first todo update). A few rows tall, full input width — Claude Code style —
/// reserved in the layout so the transcript shrinks above it instead of being
/// painted over. Glyphs: ✓ completed (dim, struck-through), ▸ in progress
/// (accent), ☐ pending.
pub(super) fn draw_todo_band(frame: &mut Frame, app: &App, area: Rect) {
    if area.height < 3 || area.width < 8 {
        return;
    }
    let (done, total) = crate::tools::todo::progress(&app.todos);

    let block = Block::bordered()
        .border_type(theme::border_type())
        .border_style(theme::style(Token::Border))
        .title(Line::from(vec![
            Span::styled(" ≡ ", accent()),
            Span::styled(format!("todos {done}/{total}"), muted()),
            Span::styled(" · esc", dim()),
        ]));
    let inner = block.inner(area);
    let inner_width = inner.width as usize;
    let visible = inner.height as usize;
    let lines: Vec<Line<'static>> = if app.todos.is_empty() {
        vec![Line::from(Span::styled("(empty)", dim().italic()))]
    } else {
        // Prefer the in-progress item and its neighbors when the list is
        // taller than the band: scroll so the current work stays visible.
        let focus = app
            .todos
            .iter()
            .position(|item| item.status == crate::tools::todo::TodoStatus::InProgress)
            .unwrap_or(0);
        let start = if app.todos.len() <= visible {
            0
        } else {
            focus
                .saturating_sub(visible.saturating_sub(1) / 2)
                .min(app.todos.len().saturating_sub(visible))
        };
        app.todos
            .iter()
            .skip(start)
            .take(visible)
            .map(|item| {
                use crate::tools::todo::TodoStatus;
                let (glyph_style, text_style) = match item.status {
                    TodoStatus::Completed => (dim(), dim().add_modifier(Modifier::CROSSED_OUT)),
                    TodoStatus::InProgress => (accent(), accent().bold()),
                    TodoStatus::Pending => (dim(), muted()),
                };
                truncate_line(
                    Line::from(vec![
                        Span::styled(format!("{} ", item.status.glyph()), glyph_style),
                        Span::styled(item.content.clone(), text_style),
                    ]),
                    inner_width,
                )
            })
            .collect()
    };
    frame.render_widget(Paragraph::new(Text::from(lines)).block(block), area);
}

/// Git diff sidebar (`/diff`): separated from the chat by a single dim
/// rule, with conventional green additions / red deletions (foreground
/// only). Lines wider than the sidebar are cut with a dim `…` instead of
/// clipping silently.
pub(super) fn draw_diff_sidebar(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::new()
        .borders(Borders::LEFT)
        .border_type(theme::border_type())
        .border_style(theme::style(Token::Border))
        .title(Line::from(vec![
            Span::styled(" ± ", accent()),
            Span::styled("git diff", muted()),
            Span::styled(" · esc closes", dim()),
        ]));
    let inner = block.inner(area);
    // A titled block takes the top row for its title, so on a one-row sidebar
    // `inner` is an empty rect sitting one row *below* the frame. The overflow
    // hint further down builds a `height: 1` rect at `inner.y` and would then
    // index a cell that does not exist — a panic, mid-draw, from nothing worse
    // than `/diff` on a very short terminal.
    if inner.height == 0 || inner.width == 0 {
        return;
    }
    let inner_width = inner.width as usize;
    let inner_height = inner.height as usize;
    let Some(diff) = app.diff.as_ref() else {
        return;
    };
    let lines: Vec<Line<'static>> = highlight_diff(&diff.text)
        .lines
        .into_iter()
        .map(|line| truncate_line(line, inner_width))
        .collect();
    // Clamp the scroll to the content so PgDn can't strand the view past the
    // end; the key handler lets diff_scroll grow unbounded (mirroring the
    // transcript), and render is the single source of truth for the bound.
    let max_scroll = lines.len().saturating_sub(inner_height);
    let scroll = (diff.scroll as usize).min(max_scroll);
    // The row the hint below would sit on, measured before the move.
    let top_row_width = lines
        .get(scroll)
        .map(|line| line.width() as u16)
        .unwrap_or(0);
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .scroll((scroll as u16, 0))
            .block(block),
        area,
    );
    // Quiet "↕ N more" hint in the top-right when the diff overflows, so it's
    // discoverable that there's more below (and that PgUp/PgDn page it).
    if max_scroll > 0 {
        let remaining = max_scroll - scroll;
        if remaining > 0 {
            let label = format!(" ↓ {remaining} more ");
            let label_width = label.width() as u16;
            // Only when the first visible diff line leaves room. This one did
            // not even `Clear` first — it painted a `Paragraph` straight over
            // row zero, so the top line of every overflowing diff was cut
            // mid-word: "could not read git diff: gi ↓ 105 more". A diff is
            // exactly the place where silently losing characters is worst.
            if inner.width > label_width && top_row_width + label_width <= inner.width {
                let hint = Rect {
                    x: inner.x + inner.width - label_width,
                    y: inner.y,
                    width: label_width,
                    height: 1,
                };
                frame.render_widget(Paragraph::new(Line::from(Span::styled(label, dim()))), hint);
            }
        }
    }
}

/// Bottom status line: model, mode, and turn state on the left; contextual
/// key hints on the right. One quiet line, no background fill.
/// An indeterminate, indicatif-style block bar: a lit window of `█` slides
/// across a dim `░` track, wrapping. Driven by `tick` so it animates frame to
/// frame without knowing a total (compaction is one opaque LLM call).
pub(super) fn indeterminate_bar(width: usize, tick: u64) -> Line<'static> {
    let width = width.max(4);
    let window = (width / 5).max(3);
    let offset = (tick as usize) % width;
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut run = String::new();
    let mut run_lit: Option<bool> = None;
    for i in 0..width {
        // Lit cells are the `window` columns starting at `offset`, wrapping.
        let lit = (i + width - offset) % width < window;
        if run_lit != Some(lit) {
            if let Some(prev) = run_lit {
                let style = if prev { accent() } else { dim() };
                spans.push(Span::styled(std::mem::take(&mut run), style));
            }
            run_lit = Some(lit);
        }
        run.push(if lit { '█' } else { '░' });
    }
    if let Some(prev) = run_lit {
        let style = if prev { accent() } else { dim() };
        spans.push(Span::styled(run, style));
    }
    Line::from(spans)
}

/// The turn's status on its own row above the composer, indented to the
/// composer's own left inset so the two line up. Codex's placement.
pub(super) fn draw_status_above(frame: &mut Frame, app: &App, area: Rect) {
    let elapsed = app
        .turn_started
        .map(|started| started.elapsed().as_secs())
        .unwrap_or(0);
    let mut spans = vec![Span::raw(" ".repeat(skin::LIVE_PREFIX_COLS))];
    spans.extend(busy_spans(app, elapsed));
    frame.render_widget(
        Paragraph::new(truncate_line(Line::from(spans), area.width as usize)),
        area,
    );
}

/// What sits between two status-line chips. A middle dot under most skins, two
/// spaces under `codex`, whose footer separates by whitespace alone.
pub(super) fn sep() -> Span<'static> {
    Span::styled(skin::chrome().separator, dim())
}

/// How the active skin narrates a turn in flight.
///
/// The *information* is the same under all four — this is Wizard's turn, and
/// the step counter is Wizard's own idea (nothing else here has a step
/// budget) — so what a skin changes is the phrasing, not the facts. A skin
/// that hid the step count to look more like the thing it is imitating would
/// be lying about which agent the user is running.
pub(super) fn busy_spans(app: &App, elapsed: u64) -> Vec<Span<'static>> {
    // Capped budget shows the denominator; the default unlimited budget has
    // none to show, so the step is just a count.
    let step = match app.status.max_steps.cap() {
        Some(cap) => format!("step {}/{cap}", app.status.step),
        None => format!("step {}", app.status.step),
    };
    match skin::chrome().busy {
        BusyStyle::Steps => vec![Span::styled(format!("{step} · {elapsed}s"), dim())],
        BusyStyle::Working => {
            let mut spans = skin::motion::shimmer("Working", app.tick);
            spans.push(Span::raw(" "));
            spans.push(Span::styled(
                format!("({step} • {elapsed}s • esc to interrupt)"),
                dim(),
            ));
            spans
        }
        BusyStyle::Thinking => vec![
            Span::styled("Thinking… ", theme::style(Token::Text)),
            Span::styled(format!("{step} · {elapsed}s · ctrl+c to stop"), dim()),
        ],
    }
}

pub(super) fn draw_status_bar(frame: &mut Frame, app: &App, area: Rect, suggestions_shown: bool) {
    // A terminal too short to hold the layout does not get a smaller status
    // bar; it gets none. `Layout` satisfies what it can and hands back
    // zero-height rects for the rest, and a zero-height rect's `y` is one row
    // past the bottom of the frame — so the right-aligned hint below, which
    // builds a `height: 1` sub-rect at `area.y`, addressed a cell outside the
    // buffer and ratatui panicked on the index. A panic here is not a bad
    // frame: it unwinds the draw, fires the panic hook that tears the terminal
    // down, and reads to the user as Wizard dying the moment they dragged
    // their window a little too small.
    if area.height == 0 || area.width == 0 {
        return;
    }
    // Compaction owns the status line while it runs: a label plus the animated
    // bar, full width.
    if app.compacting {
        let label = " compacting… ";
        let bar_width = (area.width as usize)
            .saturating_sub(label.width() + 1)
            .max(4);
        let mut spans = vec![Span::styled(label, accent().bold())];
        spans.extend(indeterminate_bar(bar_width, app.tick).spans);
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
        return;
    }
    let spinner = spinner_frame(app.tick);
    // What is on the line, and why (`docs/design.md`): the model, then only
    // the states that are true right now, then where you are and what the
    // next call costs. The default mode and the working directory are not
    // here: `genie` is the default and says nothing, and the branch names the
    // repo (`/status` has the path).
    let mut spans = vec![Span::raw(" "), model_span(app)];
    if app.status.mode == Mode::Sovereign {
        spans.push(sep());
        spans.push(mode_span(app.status.mode));
    }
    // Vim mode indicator: NORMAL stands out (bold accent), INSERT stays quiet.
    if let Some(label) = app.vim.label() {
        spans.push(sep());
        let style = if app.vim.mode == VimMode::Normal {
            accent().bold()
        } else {
            dim()
        };
        spans.push(Span::styled(label, style));
    }
    if app.omakase {
        spans.push(sep());
        spans.push(Span::styled("OMAKASE", accent().bold()));
    } else if app.plan_mode {
        spans.push(sep());
        spans.push(Span::styled("PLAN", accent().bold()));
    }
    // Ultra is the one sticky mode that changes neither the model string nor the
    // mode word, so it needs a chip of its own — and the chip is the candidate
    // count, because that is the multiplier on what the next turn costs.
    if let Some(ultra) = &app.ultra {
        spans.push(sep());
        spans.push(Span::styled(
            format!("ULTRA \u{00d7}{}", ultra.candidates()),
            accent().bold(),
        ));
    }
    if let Some(branch) = git_branch(&app.project_root) {
        spans.push(sep());
        spans.push(Span::styled(truncate_width(&branch, 24), dim()));
    }
    // Context meter: tokens that will load into the next model call — last
    // reported prompt size, or a post-compact / post-clear estimate. Not the
    // session-lifetime sum (that double-counts multi-step history and stays
    // inflated after /clear).
    if app.status.context_tokens > 0 {
        spans.push(sep());
        spans.push(Span::styled(
            crate::usage::format_tokens(app.status.context_tokens),
            dim(),
        ));
    }
    if let Some(cost) = session_cost(app) {
        spans.push(sep());
        spans.push(Span::styled(cost, dim()));
    }
    if let Some(label) = &app.rebuilding {
        spans.push(sep());
        spans.push(Span::styled(format!("{spinner} "), accent()));
        spans.push(Span::styled(format!("{label}…"), dim().italic()));
    }
    // Background tasks (`/bashes`): a persistent marker while any are
    // running, so a detached command doesn't silently vanish from view.
    if app.status.background_tasks > 0 {
        spans.push(sep());
        spans.push(Span::styled(
            format!(
                "⏵ {} bg task{}",
                app.status.background_tasks,
                if app.status.background_tasks == 1 {
                    ""
                } else {
                    "s"
                }
            ),
            accent(),
        ));
    }
    // Backgrounded subagents (`spawn_subagent` with `background: true`):
    // same persistent marker, so a delegated task stays visible while the
    // user is free to keep talking.
    if app.status.background_subagents > 0 {
        spans.push(sep());
        spans.push(Span::styled(
            format!(
                "⏵ {} bg subagent{}",
                app.status.background_subagents,
                if app.status.background_subagents == 1 {
                    ""
                } else {
                    "s"
                }
            ),
            accent(),
        ));
    }
    // MCP is still connecting in the background: a transient marker, shown
    // alongside the busy/step indicator (a turn can start before tools arrive)
    // so the missing-tools window isn't a silent surprise. Vanishes when the
    // connect finishes.
    if app.mcp_connecting {
        spans.push(sep());
        spans.push(Span::styled(format!("{spinner} "), accent()));
        spans.push(Span::styled("connecting tools…", dim().italic()));
    }
    // A failed health probe leaves a persistent marker so the breakage survives
    // once the user starts typing and the welcome screen is gone.
    if app.provider_health_error.is_some() {
        spans.push(sep());
        spans.push(Span::styled("⚠ provider", warning().bold()));
    }
    // Truncated, with the ellipsis `truncate_line` adds. Only the right-hand
    // hint was ever given a width budget; the left side was a `Paragraph` with
    // no wrap, so ratatui hard-clipped it at the frame edge — mid-word, with no
    // sign it had happened. At 60 columns the cwd chip read
    // "…/nested-subdir-level-tw", one character short of the truth and already
    // carrying its own `…` from `format_cwd`, so the second cut was invisible.
    // At 20 columns the line ended " genie ·" — a separator with nothing after
    // it, which reads as a missing value rather than as a narrow terminal.
    //
    // Everything here is unbounded except the cwd: the model name, the vim,
    // plan, omakase and ultra chips, the token count, the background-task and
    // MCP markers. Any of them can be the one that overflows.
    let line = truncate_line(Line::from(spans), area.width as usize);
    let left_width = line.width() as u16;
    frame.render_widget(Paragraph::new(line), area);

    // The right side: the keys a modal state needs, or how long the turn has
    // been running. Idle shows whatever the skin's idle hint is, and the house
    // skin's is nothing.
    let busy_hint;
    let hints: &str = if let Some(review) = &app.plan_review {
        if review.feedback.is_some() {
            "type feedback · Enter reject · Esc back"
        } else {
            "y/Enter approve · n reject · ↑↓ scroll"
        }
    } else if app.interview.is_some() {
        "1-9 pick · type answer · Enter next · Esc skip"
    } else if app.picker.is_some() {
        "↑↓ move · Enter select · Esc cancel"
    } else if suggestions_shown {
        "↑↓ select · Tab complete · Enter run"
    } else if app.diff.is_some() {
        "PgUp/PgDn diff · Esc close"
    } else if app.console.is_some() {
        // Loudest of the lot, and first: while a command owns the composer,
        // Enter does something entirely different from what it does the rest of
        // the time, and the user has to be able to see that at a glance.
        "Enter → command · Ctrl-D end input · Esc detach · Ctrl-C stop"
    } else if app.status.busy {
        let elapsed = app
            .turn_started
            .map(|started| started.elapsed())
            .unwrap_or_default();
        busy_hint = match app.message_queue.len() {
            0 => fmt_elapsed(elapsed),
            n => format!("{} · queued {n}", fmt_elapsed(elapsed)),
        };
        &busy_hint
    } else {
        // The only hint a skin gets to reword: the idle one, which is the line
        // people quote when they describe what a TUI looks like ("? for
        // shortcuts"). The rest name keys that do something specific right now,
        // and paraphrasing those to match somebody else's product would make
        // them harder to act on, which is the opposite of what a hint is for.
        skin::chrome().idle_hint
    };
    if hints.is_empty() {
        return;
    }
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

/// The composer's top rule while a command owns it: a labelled band naming
/// what Enter is typing into.
///
/// Drawn in place of the plain rule rather than as an extra row, so switching
/// modes never reflows the transcript above — a console opens and closes at
/// whatever moment the agent decides to run a command, and a composer that
/// grew a line each time would make the screen jump under the reader.
pub(super) fn console_rule(command: &str, width: u16) -> Line<'static> {
    // Bounded by the rule, not just by 48 columns of command.
    //
    // `used` clamped the *arithmetic* to `width` while `label` was pushed
    // whole, so on a terminal narrower than the label the rule overflowed: the
    // trailing fill was lost and the command name was cut by the frame with no
    // ellipsis. " ▶ stdin → " plus 48 columns reaches about 59, so any pane
    // under ~60 columns running a command hit it — and this rule exists to say
    // that Enter now goes somewhere different, which is the worst line to
    // render unreadably.
    //
    // Two columns held back for the leading dash and one trailing cell.
    let label = truncate_width(
        &format!(" ▶ stdin → {} ", truncate_width(command, 48)),
        (width as usize).saturating_sub(2),
    );
    let used = label.width().min(width as usize);
    let fill = (width as usize).saturating_sub(used + 1);
    Line::from(vec![
        Span::styled("─", dim()),
        Span::styled(label, warning().bold()),
        Span::styled("─".repeat(fill), dim()),
    ])
}

/// Columns available for composer text: two for the prompt glyph, one spare so
/// the caret can sit just past a full row, and then whatever the active skin's
/// frame costs — one column of left padding under `Rules` and `Bare`, two
/// border columns under `Boxed`.
///
/// Skin-aware because [`regions`] sizes the composer from this and
/// [`draw_input`] wraps the draft with it. If the two disagreed by a column,
/// a full row would wrap one character early under a boxed composer and the
/// caret would sit outside the box.
pub(super) fn composer_budget(width: u16) -> usize {
    let frame = match skin::chrome().composer {
        ComposerFrame::Rules | ComposerFrame::Bare => 1,
        ComposerFrame::Boxed => 2,
    };
    (width as usize).saturating_sub(3 + frame).max(1)
}

/// The composer's top row: the rule, the console banner, or nothing.
pub(super) fn top_row(area: Rect) -> Rect {
    Rect { height: 1, ..area }
}

/// The rows between a composer's top and bottom rows.
pub(super) fn inset(area: Rect) -> Rect {
    Rect {
        y: area.y + 1,
        height: area.height.saturating_sub(2),
        ..area
    }
}

/// The composer buffer as chars for layout. In the inline provider-setup
/// prompt the API-key field is masked: each typed character renders as a
/// width-1 bullet (so the cursor math is unaffected) and the real key never
/// reaches the screen.
pub(super) fn composer_chars(app: &App) -> Vec<char> {
    if app.prompt_is_masked() {
        vec!['•'; app.input.chars().count()]
    } else {
        app.input.chars().collect()
    }
}

/// Soft-wrap the composer buffer at `budget` display columns. Each visual row
/// is the half-open char range `[start, end)`: the buffer splits on hard line
/// breaks (a '\n' belongs to no row), then each logical line packs greedily by
/// display width. Wide chars never split across rows, and every row keeps at
/// least one char so a pathological budget cannot loop.
pub(super) fn wrap_rows(chars: &[char], budget: usize) -> Vec<(usize, usize)> {
    let budget = budget.max(1);
    let breaks = chars
        .iter()
        .enumerate()
        .filter(|&(_, &c)| c == '\n')
        .map(|(i, _)| i);
    let mut rows = Vec::new();
    let mut ls = 0usize;
    for le in breaks.chain(std::iter::once(chars.len())) {
        let mut rs = ls;
        let mut used = 0usize;
        for (i, c) in chars.iter().enumerate().take(le).skip(ls) {
            let w = c.width().unwrap_or(0);
            if i > rs && used + w > budget {
                rows.push((rs, i));
                rs = i;
                used = 0;
            }
            used += w;
        }
        rows.push((rs, le));
        ls = le + 1;
    }
    rows
}

/// Map a cursor (char offset) to its visual (row, column-in-chars) position
/// in `rows` from [`wrap_rows`]. A cursor exactly on a soft-wrap boundary
/// belongs to the start of the next visual row (that is where the next char
/// would land); at a hard break or end of text it stays at the end of its row.
pub(super) fn cursor_visual(rows: &[(usize, usize)], cursor: usize) -> (usize, usize) {
    for (ri, &(rs, re)) in rows.iter().enumerate() {
        // The next row continues this logical line iff it starts where this
        // row ends (a hard break consumes the '\n', leaving a gap of one).
        let continues = rows.get(ri + 1).is_some_and(|&(ns, _)| ns == re);
        if cursor < re || (cursor == re && !continues) {
            return (ri, cursor.saturating_sub(rs));
        }
    }
    (rows.len().saturating_sub(1), 0)
}

/// Input: an accent prompt inside whichever frame the active skin asks for —
/// dim rules above and below (`wizard`), a box (`claude`, `grok`), or nothing
/// at all (`codex`). Soft-wraps long lines onto continuation rows and handles
/// inline ghost-text completion.
pub(super) fn draw_input(frame: &mut Frame, app: &App, area: Rect) {
    if area.width < 6 {
        return;
    }
    let chrome = skin::chrome();
    // Too short for the full composer — rule, input, rule — but not too short
    // to say where the prompt is. Drawing nothing meant that on a terminal
    // three or four rows tall the whole bottom of the screen was blank while
    // the status bar carried on rendering normally: no rules, no `❯`, no
    // caret. It looks like the application has hung, and there is nothing on
    // screen to suggest the window is simply too small.
    //
    // So the last row of whatever there is gets the prompt and the draft. It
    // is not a composer, but it is honest about being the composer.
    if area.height < 3 {
        if area.height == 0 {
            return;
        }
        let row = Rect {
            x: area.x,
            y: area.bottom() - 1,
            width: area.width,
            height: 1,
        };
        let draft = truncate_line(
            Line::from(vec![
                Span::raw(" "),
                Span::styled(chrome.prompt, accent()),
                Span::styled(app.input.clone(), theme::style(Token::Text)),
            ]),
            area.width as usize,
        );
        frame.render_widget(Paragraph::new(draft), row);
        return;
    }
    // The frame, and the rectangle the draft is written into. Every frame is
    // two rows tall so `regions` can size the composer without knowing which
    // skin is on; what differs is what those two rows are made of.
    let (inner, pad) = match chrome.composer {
        // A dim rule above and below, no box, and one column of left padding
        // that keeps the prompt aligned with the transcript margin.
        ComposerFrame::Rules => {
            let rule = Line::from(Span::styled("─".repeat(area.width as usize), dim()));
            // While a command owns the composer the top rule carries its name.
            // This is the loud half of saying which mode Enter is in: a hint in
            // the corner is easy to miss, a banner spanning the composer is
            // not, and a composer that silently means something else is worse
            // than the bug consoles fix.
            let head = match &app.console {
                Some(console) => console_rule(&console.command, area.width),
                None => rule.clone(),
            };
            // One rule, above: it says where the transcript stops scrolling.
            // The row below the draft is left blank; the status line is the
            // boundary there and a second rule would only repeat it.
            frame.render_widget(Paragraph::new(Text::from(vec![head])), top_row(area));
            (inset(area), 1usize)
        }
        // A box in the theme's border style. The console banner becomes its
        // title, for the same reason it replaces the rule above: it has to be
        // impossible to miss and must not reflow the transcript when it
        // appears.
        ComposerFrame::Boxed => {
            let mut block = Block::bordered()
                .border_type(theme::border_type())
                .border_style(theme::style(Token::Border));
            if let Some(console) = &app.console {
                block = block.title(Line::from(vec![
                    Span::styled(" ▶ stdin → ", warning().bold()),
                    Span::styled(
                        format!("{} ", truncate_width(&console.command, 32)),
                        warning(),
                    ),
                ]));
            }
            let inner = block.inner(area);
            frame.render_widget(block, area);
            // One column of inner padding: a prompt glyph flush against the
            // border reads as a rendering fault rather than as a box.
            (inner, 1usize)
        }
        // Nothing at all: the glyph hangs in the margin and the draft sits on
        // the terminal's own background, with a blank row above and below so
        // it does not collide with the transcript or the hints.
        ComposerFrame::Bare => {
            if let Some(console) = &app.console {
                frame.render_widget(
                    Paragraph::new(Line::from(vec![
                        Span::styled(" ▶ stdin → ", warning().bold()),
                        Span::styled(truncate_width(&console.command, 48), warning()),
                    ])),
                    top_row(area),
                );
            }
            (inset(area), 0usize)
        }
    };
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let prompt_width = 2usize;
    let budget = composer_budget(area.width);

    let chars = composer_chars(app);
    let cursor = app.cursor.min(chars.len());
    let normal = app.vim.is_normal();

    let rows = wrap_rows(&chars, budget);
    let (crow, ccol) = cursor_visual(&rows, cursor);

    // Vertical window: show a block of rows that keeps the cursor row in view.
    let content_h = (inner.height as usize).max(1);
    let voff = if crow < content_h {
        0
    } else {
        crow - content_h + 1
    };
    let last = (voff + content_h).min(rows.len());

    let block = Style::default().add_modifier(Modifier::REVERSED);
    let mut lines: Vec<Line> = Vec::new();
    let mut cursor_xy: Option<(u16, u16)> = None;

    for ri in voff..last {
        let (rs, re) = rows[ri];
        let row: &[char] = &chars[rs..re];
        let widths: Vec<usize> = row.iter().map(|c| c.width().unwrap_or(0)).collect();
        let is_cursor_row = ri == crow;

        // First row carries the prompt glyph; continuation rows (wrapped or
        // hard-broken) indent to match.
        let leading = if ri > 0 {
            Span::raw("  ")
        } else if app.console.is_some() {
            // A different glyph in a different colour, because the line being
            // composed is going somewhere else entirely.
            Span::styled("▶ ", warning().bold())
        } else {
            Span::styled(chrome.prompt, accent().bold())
        };
        let mut spans = vec![Span::raw(" ".repeat(pad)), leading];

        if normal && is_cursor_row {
            // Vim Normal mode paints its own block cursor (reversed cell) so the
            // mode is legible without a hardware caret.
            let rel = ccol.min(row.len());
            spans.push(Span::raw(row[..rel].iter().collect::<String>()));
            if rel < row.len() {
                spans.push(Span::styled(row[rel].to_string(), block));
                spans.push(Span::raw(row[rel + 1..].iter().collect::<String>()));
            } else {
                spans.push(Span::styled(" ", block));
            }
        } else {
            spans.push(Span::raw(row.iter().collect::<String>()));

            // Ghost text (command completion) only makes sense on a single-row
            // line with the cursor at the very end, where → can accept it.
            if is_cursor_row
                && !normal
                && rows.len() == 1
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
                    let used: usize = widths.iter().sum();
                    let room = budget.saturating_sub(used);
                    if !ghost.is_empty() && room > 0 {
                        let ghost: String = ghost.chars().take(room).collect();
                        spans.push(Span::styled(ghost, dim().italic()));
                    }
                }
            }

            if is_cursor_row && !normal {
                let cols: usize = widths[..ccol.min(widths.len())].iter().sum();
                let x = inner.x + (pad + prompt_width) as u16 + cols as u16;
                let y = inner.y + (ri - voff) as u16;
                cursor_xy = Some((x, y));
            }
        }
        lines.push(Line::from(spans));
    }

    frame.render_widget(Paragraph::new(Text::from(lines)), inner);

    // In Normal mode the block cursor above is the only cursor; otherwise place
    // the terminal's caret on the cursor row.
    if !normal
        && app.picker.is_none()
        && app.plan_review.is_none()
        && app.interview.is_none()
        && let Some((x, y)) = cursor_xy
    {
        frame.set_cursor_position(Position::new(x, y));
    }
}

/// Where the suggestion popup goes, or `None` when there is no room for it.
///
/// One function because two callers need the same answer: `draw_suggestions`,
/// to draw it, and the status bar, to decide whether to advertise it. They used
/// to disagree — the status bar keyed off `!app.suggestions.is_empty()` while
/// the popup bailed on a too-small rect, so a six-row terminal showed
/// "↑↓ select · Tab complete · Enter run" with no list anywhere on screen and
/// three keys that did nothing visible.
pub(super) fn suggestion_area(app: &App, input_area: Rect, frame_area: Rect) -> Option<Rect> {
    if app.suggestions.is_empty() || overlay_open(app) {
        return None;
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
    .intersection(frame_area);
    (area.height >= 3 && area.width >= 4).then_some(area)
}

/// Command-suggestion popup floating directly above the input rule.
pub(super) fn draw_suggestions(frame: &mut Frame, app: &App, input_area: Rect) {
    let Some(area) = suggestion_area(app, input_area, frame.area()) else {
        return;
    };
    frame.render_widget(Clear, area);

    let usage_width = app
        .suggestions
        .iter()
        .map(|spec| spec.name.len() + spec.args.len() + 2)
        .max()
        .unwrap_or(0);
    let inner_width = area.width.saturating_sub(2) as usize;
    // The usage column is the widest usage over *every* command, and at a
    // narrow width that is wider than the popup. It was only ever padded, never
    // clipped, so `/effort [low|medium|high|d` ran flush into the right border
    // with no ellipsis while every other row was space-padded. Bound it to
    // what there is, leaving the marker and a gap.
    let usage_width = usage_width.min(inner_width.saturating_sub(4));
    // Columns left for the description: marker + padded usage + gap.
    let description_room = inner_width.saturating_sub(usage_width + 5);
    // Below a few columns a description is not shortened, it is erased:
    // `truncate_width(desc, 0)` still emits the ellipsis, so a 40-column popup
    // showed a whole column of lone `…` glyphs — noise standing exactly where
    // the answer should be. Under that, drop the column and give the room to
    // the command names, which are the part you are choosing between.
    let show_description = description_room >= 8;

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
                ("  ", muted())
            };
            let usage = truncate_width(&format!("/{} {}", spec.name, spec.args), usage_width);
            let mut spans = vec![
                Span::styled(marker, accent()),
                Span::styled(format!("{usage:<usage_width$}"), name_style),
            ];
            if show_description {
                spans.push(Span::styled(
                    format!("  {}", truncate_width(&spec.description, description_room)),
                    dim(),
                ));
            }
            Line::from(spans)
        })
        .collect();

    let block = Block::bordered()
        .border_type(theme::border_type())
        .border_style(theme::style(Token::Border));
    frame.render_widget(Paragraph::new(Text::from(lines)).block(block), area);
}

/// Centered modal for the model / mode / rewind / subagent picker.
pub(super) fn draw_picker(frame: &mut Frame, app: &App) {
    let Some(picker) = &app.picker else {
        return;
    };

    let frame_area = frame.area();
    // Centred in the *transcript*, not in the frame.
    //
    // It used to reserve a flat six rows off the frame's height and centre in
    // what was left, which is not what the bottom of the screen costs: the
    // composer alone is `input_rows + 2` and grows with a wrapped draft, and
    // the todo band, the subagent rail and the status line are all below the
    // transcript too. Once a list was long enough to hit that cap the box ran
    // two rows into the composer — the picker's footer and the composer's top
    // rule printed on the same line, `╰─ ↑↓ move · Enter select · Esc cancel ──╯`
    // straddling ` ❯ `. `regions` already knows where the transcript ends, and
    // it is the one place that knows, so ask it instead of guessing.
    let body = regions(app, frame_area).body;
    let width = (body.width.saturating_sub(8)).clamp(24, 56);
    // Two rows for the border. One more so a full-height picker does not sit
    // flush against the transcript's edges.
    let max_rows = body.height.saturating_sub(3).max(1) as usize;
    let height = picker.items.len().min(max_rows) as u16 + 2;
    let area = Rect {
        x: body.x + (body.width.saturating_sub(width)) / 2,
        y: body.y + (body.height.saturating_sub(height)) / 2,
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
                muted()
            };
            // Ellipsize long model tags so the current marker stays visible.
            let suffix = if item.current { " ●".width() } else { 0 };
            let value_room = inner_width.saturating_sub(2 + suffix + 1);
            let value = truncate_width(&item.value, value_room);
            // Width consumed so far: marker (2) + value + the current-marker.
            let consumed = 2 + value.width() + suffix;
            let mut spans = vec![
                Span::styled(marker, accent()),
                Span::styled(value, value_style),
            ];
            if item.current {
                spans.push(Span::styled(" ●", accent()));
            }
            // Truncate the detail to the room left on the line (after a two-space
            // gap) so long descriptions never spill past the modal border.
            if !item.detail.is_empty() {
                let room = inner_width.saturating_sub(consumed + 2);
                if room > 0 {
                    let detail = truncate_width(&item.detail, room);
                    spans.push(Span::styled(format!("  {detail}"), dim()));
                }
            }
            Line::from(spans)
        })
        .collect();

    let block = Block::bordered()
        .border_type(theme::border_type())
        .border_style(theme::style(Token::Border))
        // Both bounded by the box. A `Block` title is clipped by ratatui with
        // no ellipsis, so at 50 columns the settings picker read
        // "╭  settings · ↑/↓ move · enter select · e╮" — "esc close" cut to a
        // lone "e" running into the corner glyph, which looks like a drawing
        // bug rather than a title too long for its box. Two columns for the
        // corners, two more so the title never touches them.
        .title(Line::from(vec![Span::styled(
            truncate_width(
                &format!(" {} ", picker.title),
                area.width.saturating_sub(4) as usize,
            ),
            muted(),
        )]))
        .title_bottom(
            Line::from(Span::styled(
                truncate_width(picker.footer_hint(), area.width.saturating_sub(4) as usize),
                dim(),
            ))
            .centered(),
        );
    frame.render_widget(Paragraph::new(Text::from(lines)).block(block), area);
}

/// A dim "· text" placeholder row for an empty modal section.
fn dash_bullet(text: &str, style: Style) -> Line<'static> {
    Line::from(Span::styled(format!("· {text}"), style))
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Compact "how long ago" label: `12s`, `4m`, `2h`, `3d`.
fn fmt_age(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

/// Machine-wide session manager (`/dashboard`): every live Wizard session on
/// the machine, grouped by state, refreshed from the registry while open.
/// Modal — ↑/↓ move the selection, Ctrl-X stops the selected session,
/// typing + Enter dispatches a background session, Esc clears the input or
/// closes. Attach arrives in a later milestone.
pub(super) fn draw_dashboard(frame: &mut Frame, app: &App) {
    let area = frame.area();
    frame.render_widget(Clear, area);

    let count = app.sessions.len();
    let block = Block::bordered()
        .border_type(theme::border_type())
        .border_style(theme::style(Token::Border))
        .title(Line::from(vec![
            Span::styled(
                " wizard sessions",
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("  ({count} live on this machine)"), dim()),
        ]))
        .title_bottom(
            Line::from(Span::styled(" ↑↓ select · Ctrl-X stop · Esc close ", dim())).centered(),
        );
    let outer = block.inner(area);
    frame.render_widget(block, area);
    if outer.width < 8 || outer.height < 5 {
        return;
    }
    // On a wide terminal, a peek panel of the selected session sits on the
    // right; the list and dispatch input take the left.
    let (body_area, peek_area) = if outer.width >= 80 {
        let [left, right] =
            Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)])
                .areas(outer);
        (left, Some(right))
    } else {
        (outer, None)
    };
    if let Some(peek_area) = peek_area {
        draw_peek(frame, app, peek_area);
    }
    // Reserve the bottom rows for the dispatch input.
    let [inner, input_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(2)]).areas(body_area);
    let width = inner.width as usize;
    let spinner = spinner_frame(app.tick);
    let now = now_unix();

    let mut lines: Vec<Line<'static>> = Vec::new();
    if app.sessions.is_empty() {
        lines.push(dash_bullet("no running sessions", dim().italic()));
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            "every running wizard registers here — start another to see it appear",
            dim().italic(),
        )));
    } else {
        // Sessions arrive pre-sorted by state then recency; emit a group header
        // whenever the state group changes.
        let mut current_group = "";
        for (i, session) in app.sessions.iter().enumerate() {
            let group = session.state.group();
            if group != current_group {
                if !lines.is_empty() {
                    lines.push(Line::raw(""));
                }
                lines.push(Line::from(Span::styled(
                    group.to_string(),
                    accent().add_modifier(Modifier::BOLD),
                )));
                current_group = group;
            }

            let selected = i == app.dashboard_selected;
            let marker = if selected { "❯ " } else { "  " };
            let (icon, icon_style) = match session.state {
                SessionState::Working => (spinner.to_string(), theme::style(Token::ToolRunning)),
                SessionState::NeedsInput => ("?".to_string(), accent().bold()),
                SessionState::Idle => ("·".to_string(), dim()),
                SessionState::Completed => ("✓".to_string(), theme::style(Token::ToolDone)),
                SessionState::Failed => ("✗".to_string(), theme::style(Token::ToolFailed).bold()),
            };
            let name_style = if selected {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                muted()
            };
            // Mark this very session so the user can spot which row is them.
            let you = if session.id == app.session_id {
                " (this one)"
            } else {
                ""
            };
            let age = fmt_age(now.saturating_sub(session.updated_unix));
            lines.push(truncate_line(
                Line::from(vec![
                    Span::styled(marker, accent()),
                    Span::styled(format!("{icon} "), icon_style),
                    Span::styled(format!("{}{you}", session.name), name_style),
                    Span::styled(format!("  {}", session.activity), dim()),
                    Span::styled(format!("  · {} · {age}", session.mode), dim()),
                ]),
                width,
            ));
        }
    }

    let max = inner.height as usize;
    if lines.len() > max {
        lines.truncate(max);
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);

    // Dispatch input: type a task + Enter to spawn a background session.
    let prompt_line = truncate_line(
        Line::from(vec![
            Span::styled("› ", accent()),
            if app.dashboard_input.is_empty() {
                Span::styled("dispatch a task…", dim().italic())
            } else {
                Span::styled(app.dashboard_input.clone(), code())
            },
        ]),
        input_area.width as usize,
    );
    let hint = Line::from(Span::styled(
        "Enter dispatch · type to compose",
        dim().italic(),
    ));
    frame.render_widget(
        Paragraph::new(Text::from(vec![prompt_line, hint])),
        input_area,
    );
}

/// The dashboard's peek panel: the selected session's recent transcript,
/// role-prefixed, pinned to the latest output. Read-only.
fn draw_peek(frame: &mut Frame, app: &App, area: Rect) {
    let title = app
        .sessions
        .get(app.dashboard_selected)
        .map(|session| format!(" peek · {} ", session.name))
        .unwrap_or_else(|| " peek ".to_string());
    let pblock = Block::new()
        .borders(Borders::LEFT)
        .border_type(theme::border_type())
        .border_style(theme::style(Token::Border))
        .title(Line::from(Span::styled(title, accent())));
    let pinner = pblock.inner(area);
    frame.render_widget(pblock, area);
    if pinner.width < 2 || pinner.height < 1 {
        return;
    }
    let pwidth = pinner.width as usize;
    let height = pinner.height as usize;

    if app.peek_lines.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled("(no transcript yet)", dim().italic())),
            pinner,
        );
        return;
    }

    // Build only the visible tail: walk messages newest-first, emit each
    // message's lines bottom-up, and stop once the panel is full. This keeps
    // rendering O(panel height) instead of O(whole transcript).
    let mut lines: Vec<Line<'static>> = Vec::new();
    'outer: for (role, text) in app.peek_lines.iter().rev() {
        let role_style = match role.as_str() {
            "user" => accent().add_modifier(Modifier::BOLD),
            "assistant" => muted().add_modifier(Modifier::BOLD),
            _ => dim().add_modifier(Modifier::BOLD),
        };
        let mut block: Vec<Line<'static>> =
            vec![Line::from(Span::styled(role.clone(), role_style))];
        for line in text.lines() {
            block.push(truncate_line(
                Line::from(Span::styled(line.to_string(), muted())),
                pwidth,
            ));
        }
        for line in block.into_iter().rev() {
            lines.push(line);
            if lines.len() >= height {
                break 'outer;
            }
        }
    }
    lines.reverse();
    frame.render_widget(Paragraph::new(Text::from(lines)), pinner);
}

/// Most rail rows drawn at once. Past this the rail scrolls around the
/// selection rather than eating the transcript.
const MAX_RAIL_ROWS: usize = 5;

/// Rows the rail needs: one per subagent, capped, plus a row for the "+N more"
/// marker when it is capped. Zero when nothing has been delegated — the rail
/// costs no screen space until there is something to show.
pub(super) fn rail_height(app: &App) -> u16 {
    if app.panes.is_empty() {
        return 0;
    }
    let shown = app.panes.len().min(MAX_RAIL_ROWS);
    let overflow = usize::from(app.panes.len() > MAX_RAIL_ROWS);
    (shown + overflow) as u16
}

/// The subagent rail: one dot per run, directly under the composer.
///
/// ```text
///   ◉ researcher   read_file                     0:12 +3
/// ❯ ● reviewer     Checking token expiry…        0:04
///   ✔ tester       214 passed                    1:31 +1
/// ```
///
/// ↓ from the composer focuses it, ↑/↓ move, Enter opens the selected run as a
/// full chat view. `❯` marks the selection while the rail has focus. `+N` is
/// the unread count: what that subagent did while you were looking elsewhere.
pub(super) fn draw_rail(frame: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 || area.width < 8 {
        return;
    }
    let focused = app.rail_focus;
    let selected = focused.or(app.attached);

    // Scroll the window so the selection stays visible once there are more
    // runs than rows.
    let visible = app.panes.len().min(MAX_RAIL_ROWS);
    let start = match selected {
        Some(index) if index >= visible => index + 1 - visible,
        _ => 0,
    };
    let end = (start + visible).min(app.panes.len());

    let mut lines: Vec<Line<'static>> = Vec::new();
    for (index, pane) in app.panes.iter().enumerate().take(end).skip(start) {
        let is_selected = selected == Some(index);
        // Only the *focused* rail shows a cursor; when focus is in the
        // composer the rail is just a status readout.
        let cursor = if is_selected && focused.is_some() {
            "❯"
        } else {
            " "
        };
        // Bold on failure, like the other two ✗ sites in this file: under a
        // monochrome theme the token alone is one gray among others, and a
        // failed run has to read as failed without a hue (the glyph carries
        // the rest of the meaning: ● running, ✔ done, ✗ failed).
        let dot_style = match pane.status {
            PaneStatus::Running => theme::style(Token::ToolRunning),
            PaneStatus::Done => theme::style(Token::ToolDone),
            PaneStatus::Failed => theme::style(Token::ToolFailed).bold(),
        };
        let name_style = if is_selected { accent().bold() } else { dim() };

        let elapsed = pane.elapsed().as_secs();
        let clock = format!("{}:{:02}", elapsed / 60, elapsed % 60);
        let unread = if pane.unread > 0 && Some(index) != app.attached {
            format!(" +{}", pane.unread)
        } else {
            String::new()
        };

        // Name column is fixed-width so the activity text lines up down the
        // rail and reads as a column, not a ragged list.
        let name = truncate_width(&pane.name, 12);
        let meta_width = clock.len() + unread.len() + 4;
        let activity_width = (area.width as usize).saturating_sub(18 + meta_width).max(8);
        let activity = truncate_width(
            pane.activity().trim().lines().next().unwrap_or(""),
            activity_width,
        );

        lines.push(Line::from(vec![
            Span::styled(format!(" {cursor} "), accent()),
            Span::styled(format!("{} ", pane.glyph(app.tick)), dot_style),
            Span::styled(format!("{name:<12} "), name_style),
            Span::styled(format!("{activity:<activity_width$} "), dim()),
            Span::styled(clock, dim()),
            Span::styled(unread, accent().bold()),
        ]));
    }

    if app.panes.len() > MAX_RAIL_ROWS {
        let hidden = app.panes.len() - visible;
        lines.push(Line::from(Span::styled(
            format!("   +{hidden} more"),
            dim().italic(),
        )));
    }

    frame.render_widget(Paragraph::new(Text::from(lines)), area);
}

/// A subagent's pane: its own conversation, rendered with the same machinery as
/// the main chat, under a header naming the run. Esc goes back.
pub(super) fn draw_pane(frame: &mut Frame, app: &App, pane: &SubagentPane, area: Rect) {
    // The pane owns the screen, so no main-transcript card is clickable.
    app.card_hits.borrow_mut().clear();

    let [header_area, body_area] =
        Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).areas(area);

    let status = match pane.status {
        PaneStatus::Running => ("running", theme::style(Token::ToolRunning)),
        PaneStatus::Done => ("done", theme::style(Token::ToolDone)),
        // Bold for the same reason the rail dot is: the word has to carry
        // under a theme whose `tool.failed` is not a hue.
        PaneStatus::Failed => ("failed", theme::style(Token::ToolFailed).bold()),
    };
    let elapsed = pane.elapsed().as_secs();
    let steps = if pane.steps == 1 {
        "1 step".to_string()
    } else {
        format!("{} steps", pane.steps)
    };
    let mut header = vec![
        Span::styled(" ▌ ", accent()),
        Span::styled(pane.name.clone(), accent().bold()),
        Span::styled(" · ", dim()),
        Span::styled(status.0, status.1),
        Span::styled(
            format!(" · {}:{:02} · {steps}", elapsed / 60, elapsed % 60),
            dim(),
        ),
    ];
    if pane.bg.is_none() {
        // Worth flagging: the parent turn is blocked until this one reports.
        header.push(Span::styled(" · foreground", dim().italic()));
    }
    let hint = if app.panes.len() > 1 {
        "esc back to chat · ↑↓ next agent · shift+↑↓ scroll"
    } else {
        "esc back to chat · ↑↓ scroll"
    };
    frame.render_widget(
        Paragraph::new(Text::from(vec![
            Line::from(header),
            Line::from(vec![
                Span::styled("   ", dim()),
                Span::styled(
                    truncate_width(&pane.task, area.width.saturating_sub(6) as usize),
                    dim().italic(),
                ),
                Span::styled(format!("  {hint}"), dim()),
            ]),
        ])),
        header_area,
    );

    let inner = body_area.inner(Margin {
        horizontal: 1,
        vertical: 0,
    });
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let inner_width = inner.width as usize;
    let inner_height = inner.height as usize;

    let mut cache = app.images.borrow_mut();
    // Already wrapped and decorated per block, exactly like the main
    // transcript — which is what keeps an attached pane looking like the chat
    // it came out of.
    let rendered = items_text(
        &pane.transcript,
        app.tick,
        &mut cache,
        image_box(inner),
        inner_width,
    );
    let (mut lines, mut row_tags) = (rendered.lines, rendered.tags);
    if lines.is_empty() {
        let spinner = spinner_frame(app.tick);
        lines.push(Line::from(vec![
            Span::styled(format!("{spinner} "), accent()),
            Span::styled("starting…", dim().italic()),
        ]));
    } else if pane.status == PaneStatus::Running {
        // Same live tail as the main chat, so a running pane reads as alive.
        let spinner = spinner_frame(app.tick);
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            Span::styled(format!("{spinner} "), accent()),
            Span::styled("working…", dim().italic()),
        ]));
    }
    row_tags.resize(lines.len(), RowTag::Text);

    // Stick-to-bottom like the main transcript: follow the live tail by
    // default; once the user scrolls up, hold their top-anchored offset so
    // PageUp/Shift+↑ stay put while the run keeps writing.
    let total = lines.len();
    let max_scroll = total.saturating_sub(inner_height);
    pane.transcript.max_scroll.set(max_scroll as u16);
    let start = if pane.transcript.follow || max_scroll == 0 {
        max_scroll
    } else {
        (pane.transcript.scroll as usize).min(max_scroll)
    };
    let end = (start + inner_height).min(total);
    frame.render_widget(
        Paragraph::new(Text::from(lines[start..end].to_vec())),
        inner,
    );
    // A subagent's images belong to its run, so they are drawn here — in its
    // pane — and nowhere else.
    if !overlay_open(app) {
        paint_images(
            frame,
            inner,
            &row_tags[start..end],
            &rendered.blocks,
            &mut cache,
        );
    }
}

/// Plan-review modal (plan mode): the plan markdown with a verdict footer.
/// The turn is paused inside `exit_plan` until the user answers, so this
/// floats above everything else. While rejecting, a feedback line replaces
/// the bottom edge of the body.
pub(super) fn draw_plan_review(frame: &mut Frame, app: &App) {
    let Some(review) = &app.plan_review else {
        return;
    };

    let frame_area = frame.area();
    let width = frame_area.width.saturating_sub(6).clamp(24, 100);
    let height = frame_area.height.saturating_sub(2).max(5);
    let area = Rect {
        x: frame_area.x + (frame_area.width.saturating_sub(width)) / 2,
        y: frame_area.y + (frame_area.height.saturating_sub(height)) / 2,
        width,
        height,
    }
    .intersection(frame_area);
    if area.height < 5 || area.width < 10 {
        return;
    }
    frame.render_widget(Clear, area);

    let hints = if review.feedback.is_some() {
        " feedback · Enter reject · Esc back "
    } else {
        " y approve · n reject · ↑↓ scroll "
    };
    let block = Block::bordered()
        .border_type(theme::border_type())
        .border_style(theme::style(Token::Border))
        .title(Line::from(vec![Span::styled(" plan review ", muted())]))
        .title_bottom(Line::from(Span::styled(hints, dim())).centered());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Body: the plan, wrapped and scrolled; the bottom line is reserved for
    // the feedback input while rejecting.
    let body_area = if review.feedback.is_some() {
        Rect {
            height: inner.height.saturating_sub(1),
            ..inner
        }
    } else {
        inner
    };
    if body_area.height > 0 {
        let width = body_area.width as usize;
        // Plan markdown (incl. tables) lays out to the review body width so
        // tables don't soft-wrap mid-grid.
        let lines = wrap_lines(render_markdown_at(&review.plan, width), width);
        let max_scroll = lines.len().saturating_sub(body_area.height as usize);
        let scroll = (review.scroll as usize).min(max_scroll);
        let visible: Vec<Line<'static>> = lines
            .into_iter()
            .skip(scroll)
            .take(body_area.height as usize)
            .collect();
        frame.render_widget(Paragraph::new(Text::from(visible)), body_area);
    }

    if let Some(feedback) = &review.feedback {
        let feedback_area = Rect {
            y: inner.bottom().saturating_sub(1),
            height: 1,
            ..inner
        };
        let budget =
            (feedback_area.width as usize).saturating_sub("rejection feedback ❯  ".width());
        let shown: String = feedback
            .chars()
            .rev()
            .take(budget)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("rejection feedback ❯ ", accent().bold()),
                Span::raw(shown),
                Span::styled("▍", dim()),
            ])),
            feedback_area,
        );
    }
}

/// Centered modal for the plan-mode interview: the agent's clarifying
/// questions with their answer-so-far status, and a free-text input for the
/// current question. The turn is paused inside the `interview` tool until the
/// user answers every question or dismisses the modal.
pub(super) fn draw_interview(frame: &mut Frame, app: &App) {
    let Some(interview) = &app.interview else {
        return;
    };

    let frame_area = frame.area();
    let width = frame_area.width.saturating_sub(6).clamp(24, 92);
    let height = frame_area.height.saturating_sub(2).max(5);
    let area = Rect {
        x: frame_area.x + (frame_area.width.saturating_sub(width)) / 2,
        y: frame_area.y + (frame_area.height.saturating_sub(height)) / 2,
        width,
        height,
    }
    .intersection(frame_area);
    if area.height < 5 || area.width < 10 {
        return;
    }
    frame.render_widget(Clear, area);

    let total = interview.questions.len();
    let title = format!(
        " question {} of {total} ",
        (interview.current + 1).min(total)
    );
    let block = Block::bordered()
        .border_type(theme::border_type())
        .border_style(theme::style(Token::Border))
        .title(Line::from(vec![
            Span::styled(" interview ", muted()),
            Span::styled(title, dim()),
        ]))
        .title_bottom(
            Line::from(Span::styled(
                " 1-9 pick · type answer · Enter next · Esc skip ",
                dim(),
            ))
            .centered(),
        );
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height < 2 {
        return;
    }

    // Body: every question with its status; the current one gets its options
    // and the live answer input. The input occupies the bottom line.
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (i, q) in interview.questions.iter().enumerate() {
        if i < interview.current {
            // Answered: show the question dimmed with its answer.
            let answer = interview.answers.get(i).map(String::as_str).unwrap_or("");
            let answer = if answer.trim().is_empty() {
                "(skipped)".to_string()
            } else {
                answer.to_string()
            };
            lines.push(Line::from(vec![
                Span::styled("✓ ", theme::style(Token::Success)),
                Span::styled(q.question.clone(), dim()),
            ]));
            lines.push(Line::from(Span::styled(
                format!("    {answer}"),
                dim().italic(),
            )));
        } else if i == interview.current {
            lines.push(Line::from(vec![
                Span::styled("▶ ", accent().bold()),
                Span::styled(q.question.clone(), theme::style(Token::Heading).bold()),
            ]));
            for (n, option) in q.options.iter().enumerate() {
                lines.push(Line::from(vec![
                    Span::styled(format!("    {}) ", n + 1), accent()),
                    Span::raw(option.clone()),
                ]));
            }
        } else {
            lines.push(Line::from(Span::styled(format!("  {}", q.question), dim())));
        }
    }

    let body_area = Rect {
        height: inner.height.saturating_sub(2),
        ..inner
    };
    if body_area.height > 0 {
        let wrapped = wrap_lines(Text::from(lines), body_area.width as usize);
        let skip = wrapped.len().saturating_sub(body_area.height as usize);
        let visible: Vec<Line<'static>> = wrapped.into_iter().skip(skip).collect();
        frame.render_widget(Paragraph::new(Text::from(visible)), body_area);
    }

    // Answer input on the bottom line, scrolled to keep the tail visible.
    let input_area = Rect {
        y: inner.bottom().saturating_sub(1),
        height: 1,
        ..inner
    };
    let prompt = "answer ❯ ";
    let budget = (input_area.width as usize).saturating_sub(prompt.width() + 1);
    let shown: String = interview
        .input
        .chars()
        .rev()
        .take(budget)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(prompt, accent().bold()),
            Span::raw(shown),
            Span::styled("▍", dim()),
        ])),
        input_area,
    );
}

/// Paint the image blocks whose rows are on screen, into exactly those rows.
///
/// `tags` are the rows of `inner` that survived the scroll, in order, so a block
/// half-way off the top arrives here as the run of rows it has left — and
/// [`ImageCache::draw`] is told which row of the block that run starts at. Rows
/// the scroll took are never painted, which is the whole reason an image here
/// cannot smear across the screen.
pub(super) fn paint_images(
    frame: &mut Frame,
    inner: Rect,
    tags: &[RowTag],
    blocks: &[ImageBlock],
    cache: &mut ImageCache,
) {
    let mut row = 0usize;
    while row < tags.len() {
        let RowTag::Image { slot, row: top } = tags[row] else {
            row += 1;
            continue;
        };
        // The run of this block's rows that made it onto the screen.
        let mut height = 1u16;
        loop {
            match tags.get(row + height as usize) {
                Some(RowTag::Image {
                    slot: next,
                    row: at,
                }) if *next == slot && *at == top.saturating_add(height) => {
                    height += 1;
                }
                _ => break,
            }
        }
        if let Some(block) = blocks.get(slot) {
            let at = Rect {
                x: inner.x + IMAGE_INDENT,
                y: inner.y + row as u16,
                width: block.cols.min(inner.width.saturating_sub(IMAGE_INDENT)),
                height,
            };
            cache.draw(frame.buffer_mut(), at, block, top);
        }
        row += height as usize;
    }
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
pub(super) fn wrap_lines(text: Text<'static>, width: usize) -> Vec<Line<'static>> {
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

/// Format the working directory for the status bar: abbreviate `$HOME` to
/// `~`, and when wider than `max` columns drop leading components (prefixing
/// `…/`) so the leaf directory — the part you actually care about — stays
/// visible instead of being clipped off the end.
/// The checked-out branch of `root`, read from `.git/HEAD` (following a
/// worktree's `gitdir:` pointer), or the short hash when detached. `None`
/// outside a repository. Re-read at most every two seconds: the file is tiny,
/// but the status line asks on every frame.
pub(super) fn git_branch(root: &std::path::Path) -> Option<String> {
    type Cached = (std::path::PathBuf, std::time::Instant, Option<String>);
    static CACHE: Mutex<Option<Cached>> = Mutex::new(None);
    let mut cache = CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((path, at, branch)) = cache.as_ref()
        && path == root
        && at.elapsed() < std::time::Duration::from_secs(2)
    {
        return branch.clone();
    }
    let branch = read_git_head(root);
    *cache = Some((
        root.to_path_buf(),
        std::time::Instant::now(),
        branch.clone(),
    ));
    branch
}

fn read_git_head(root: &std::path::Path) -> Option<String> {
    let dot_git = root.join(".git");
    let git_dir = if dot_git.is_file() {
        let pointer = std::fs::read_to_string(&dot_git).ok()?;
        let target = pointer.trim().strip_prefix("gitdir:")?.trim();
        let target = std::path::Path::new(target);
        if target.is_absolute() {
            target.to_path_buf()
        } else {
            root.join(target)
        }
    } else {
        dot_git
    };
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    Some(match head.strip_prefix("ref: refs/heads/") {
        Some(branch) => branch.to_string(),
        None => head.chars().take(7).collect(),
    })
}

/// The session's cost so far, when the active provider carries a rate. The
/// same arithmetic `/cost` uses, priced as all-fresh tokens because the
/// status bar mirrors only the two flat totals.
fn session_cost(app: &App) -> Option<String> {
    if app.status.prompt_tokens == 0 && app.status.completion_tokens == 0 {
        return None;
    }
    let config = &app.config;
    let provider = config
        .active_provider
        .as_ref()
        .and_then(|name| config.providers.iter().find(|p| &p.name == name))
        .or_else(|| config.providers.first())?;
    let cost = crate::usage::cost_usd(
        crate::usage::TurnTokens {
            prompt: app.status.prompt_tokens,
            completion: app.status.completion_tokens,
            cache_read: 0,
            cache_write: 0,
        },
        provider.usd_per_mtok_in,
        provider.usd_per_mtok_out,
    )?;
    Some(if cost < 0.01 {
        "<$0.01".to_string()
    } else {
        format!("${cost:.2}")
    })
}

pub(super) fn format_cwd(root: &std::path::Path, max: usize) -> String {
    format_cwd_from(root, dirs::home_dir().as_deref(), max)
}

fn format_cwd_from(root: &std::path::Path, home: Option<&std::path::Path>, max: usize) -> String {
    let full = root.display().to_string();
    let display = match home.map(|h| h.display().to_string()) {
        Some(home) if !home.is_empty() && full.starts_with(&home) => {
            format!("~{}", &full[home.len()..])
        }
        _ => full,
    };
    if display.width() <= max {
        return display;
    }
    let sep = std::path::MAIN_SEPARATOR.to_string();
    let mut parts: Vec<&str> = display.split(&sep).filter(|p| !p.is_empty()).collect();
    while parts.len() > 1 {
        parts.remove(0);
        let candidate = format!("…{sep}{}", parts.join(&sep));
        if candidate.width() <= max {
            return candidate;
        }
    }
    // A single leaf still too wide: keep its tail under a leading `…`.
    let leaf = parts.last().copied().unwrap_or(&display);
    let budget = max.saturating_sub(1);
    let tail: String = {
        let mut used = 0;
        let mut chars: Vec<char> = Vec::new();
        for ch in leaf.chars().rev() {
            let w = ch.width().unwrap_or(0);
            if used + w > budget {
                break;
            }
            used += w;
            chars.push(ch);
        }
        chars.into_iter().rev().collect()
    };
    format!("…{tail}")
}

/// Truncate to `max` display columns (not chars), appending `…` when cut.
pub(super) fn truncate_width(text: &str, max: usize) -> String {
    if text.width() <= max {
        return text.to_string();
    }
    let mut out = take_width(text, max.saturating_sub(1)).to_string();
    out.push('…');
    out
}

/// Truncate a styled line to `max` display columns, appending a dim `…`
/// when cut so clipped content is visible as such (used by the diff
/// sidebar, where long lines would otherwise just stop mid-word).
pub(super) fn truncate_line(mut line: Line<'static>, max: usize) -> Line<'static> {
    if line.width() <= max {
        return line;
    }
    let budget = max.saturating_sub(1);
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    for span in line.spans.drain(..) {
        let span_width = span.content.width();
        if used + span_width <= budget {
            used += span_width;
            spans.push(span);
            continue;
        }
        let kept = take_width(&span.content, budget - used);
        if !kept.is_empty() {
            spans.push(Span::styled(kept.to_string(), span.style));
        }
        break;
    }
    spans.push(Span::styled("…", dim()));
    line.spans = spans;
    line
}

// ---------------------------------------------------------------------------
// Syntax highlighting (syntect) — foreground colors only, never backgrounds,
// so the terminal's own background always shows through.
// ---------------------------------------------------------------------------

static SYNTECT_ASSETS: OnceLock<(SyntaxSet, Option<SyntectTheme>)> = OnceLock::new();

fn syntect_assets() -> &'static (SyntaxSet, Option<SyntectTheme>) {
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

/// Color a unified diff for the `/diff` sidebar: `diff.add` additions,
/// `diff.del` deletions, dim context. Prefix-based (not syntect) so the
/// meaning stays legible regardless of the code-highlight theme.
pub fn highlight_diff(diff: &str) -> Text<'static> {
    // Each file is named once. `diff --git`, `index`, `---` and `+++` say the
    // same name four times over, so they collapse into one row carrying the
    // `+++` side (or the `---` side when the file was deleted).
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut removed: Option<&str> = None;
    for line in diff.lines() {
        if line.starts_with("diff ") || line.starts_with("index ") {
            continue;
        }
        if let Some(name) = line.strip_prefix("--- ") {
            removed = Some(name);
            continue;
        }
        if let Some(name) = line.strip_prefix("+++ ") {
            let name = if name == "/dev/null" {
                removed.unwrap_or(name)
            } else {
                name
            };
            let name = name
                .strip_prefix("a/")
                .or_else(|| name.strip_prefix("b/"))
                .unwrap_or(name);
            lines.push(Line::from(Span::styled(
                name.to_string(),
                theme::style(Token::DiffMeta).add_modifier(Modifier::BOLD),
            )));
            continue;
        }
        lines.push(Line::from(Span::styled(
            line.to_string(),
            diff_line_style(line),
        )));
    }
    Text::from(lines)
}

/// Style for one unified-diff line, after [`highlight_diff`] has folded the
/// file headers away.
fn diff_line_style(line: &str) -> Style {
    if line.starts_with('+') {
        theme::style(Token::DiffAdd)
    } else if line.starts_with('-') {
        theme::style(Token::DiffDel)
    } else if line.starts_with("@@") {
        theme::style(Token::DiffHunk)
    } else {
        dim()
    }
}

/// Map a syntect style to ratatui, collapsing the highlighter's foreground to
/// its grayscale luminance (chat code reads as code, not as a second palette
/// competing with the theme) and keeping font modifiers. Backgrounds are
/// dropped: they would paint over the terminal transparency. Used for fenced
/// code blocks in chat; diffs go through [`highlight_diff`] instead.
///
/// The grayscale ramp is computed here rather than drawn from a token (there
/// are as many shades as syntect emits), so it is the one color in this module
/// that does not come from the theme, and therefore the one that has to be
/// handed to [`theme::adapt`] explicitly. Without that, a 16-color terminal
/// would receive 24-bit escapes for code blocks and nothing else.
fn syntect_style(style: syntect::highlighting::Style) -> Style {
    let fg = style.foreground;
    let luma = (u32::from(fg.r) * 299 + u32::from(fg.g) * 587 + u32::from(fg.b) * 114) / 1000;
    let luma = luma as u8;
    let mut out = Style::default().fg(theme::adapt(Color::Rgb(luma, luma, luma)));
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
///
/// The cached value is *styled* lines, so whatever those styles were computed
/// under is part of the key. That is the **color depth** today: the code ramp
/// is grayscale and goes through [`theme::adapt`], so a cache that ignored it
/// would hand a 16-color terminal (or the frame after a `set_color_depth`) the
/// 24-bit escapes it cannot render. The theme **name** is in the key as well,
/// which is belt and braces at present because the ramp does not move with the
/// palette; it is there because the day [`syntect_style`] reads a token, a
/// `/theme` swap would otherwise leave every code block already on screen
/// painted in the palette it was highlighted under, and a stale cache is a
/// silent failure.
fn highlight_code_block(lang: &str, code: &str) -> Vec<Line<'static>> {
    static CACHE: OnceLock<Mutex<HashMap<u64, Vec<Line<'static>>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));

    let active = theme::active();
    let mut hasher = std::hash::DefaultHasher::new();
    lang.hash(&mut hasher);
    code.hash(&mut hasher);
    active.name.hash(&mut hasher);
    active.depth().hash(&mut hasher);
    let key = hasher.finish();
    if let Ok(guard) = cache.lock()
        && let Some(lines) = guard.get(&key)
    {
        return lines.clone();
    }

    let (syntaxes, syntax_theme) = syntect_assets();
    let syntax = if lang.is_empty() {
        None
    } else {
        syntaxes.find_syntax_by_token(lang)
    };
    let lines: Vec<Line<'static>> = match (syntax, syntax_theme.as_ref()) {
        (Some(syntax), Some(syntax_theme)) => {
            let mut highlighter = HighlightLines::new(syntax, syntax_theme);
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
                        muted(),
                    )),
                })
                .collect()
        }
        _ => code
            .lines()
            .map(|line| Line::from(Span::styled(line.to_string(), muted())))
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
/// syntax-highlighted, foreground colors only). Tables lay out into at most
/// `width` display columns: columns shrink proportionally and long cells wrap
/// inside their column so a narrow terminal keeps a coherent grid instead of
/// soft-wrapping mid-row.
pub fn render_markdown_at(source: &str, width: usize) -> Text<'static> {
    render_markdown_inner(source, true, width)
}

/// Render in-flight streaming markdown: identical, except code blocks stay
/// plain so per-frame rendering stays cheap.
fn render_markdown_streaming(source: &str, width: usize) -> Text<'static> {
    render_markdown_inner(source, false, width)
}

fn render_markdown_inner(source: &str, highlight: bool, width: usize) -> Text<'static> {
    let mut renderer = MarkdownRenderer {
        highlight,
        table_width: width,
        ..MarkdownRenderer::default()
    };
    let options = Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_TABLES
        // `$…$` / `$$…$$` emit InlineMath / DisplayMath so we can turn TeX
        // into Unicode instead of dumping raw backslash soup in the TUI.
        | Options::ENABLE_MATH;
    for event in Parser::new_ext(source, options) {
        renderer.event(event);
    }
    renderer.finish()
}

/// One table cell's styled inline spans.
type CellSpans = Vec<Span<'static>>;

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
    in_table: bool,
    /// Whether the open table closed a header row (always its first row).
    table_header: bool,
    table_aligns: Vec<MdAlignment>,
    table_rows: Vec<Vec<CellSpans>>,
    table_row: Vec<CellSpans>,
    /// Max display columns a finished table may occupy. `usize::MAX` means
    /// "size to content" (tests and callers without a viewport).
    table_width: usize,
}

impl Default for MarkdownRenderer {
    fn default() -> Self {
        Self {
            lines: Vec::new(),
            current: Vec::new(),
            bold: 0,
            italic: 0,
            strike: 0,
            code_block: false,
            highlight: false,
            code_lang: String::new(),
            code_buffer: String::new(),
            heading: false,
            lists: Vec::new(),
            quote_depth: 0,
            link: None,
            in_table: false,
            table_header: false,
            table_aligns: Vec::new(),
            table_rows: Vec::new(),
            table_row: Vec::new(),
            // Size to content unless the caller sets a viewport budget.
            table_width: usize::MAX,
        }
    }
}

impl MarkdownRenderer {
    fn style(&self) -> Style {
        // Body prose starts on the `text` token, which both built-in themes
        // leave at the terminal's own foreground; a theme *may* repaint it,
        // but neither shipped one does.
        let mut style = theme::style(Token::Text);
        if self.code_block {
            // In-flight (unhighlighted) block code: neutral, not loud.
            return muted();
        }
        if self.heading {
            style = theme::style(Token::Heading).add_modifier(Modifier::BOLD);
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
            style = style.fg(theme::color(Token::Quote));
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
                    .push(Span::styled(code.to_string(), theme::style(Token::Code)));
            }
            MdEvent::InlineMath(tex) => self.push_math(&tex, false),
            MdEvent::DisplayMath(tex) => self.push_math(&tex, true),
            // Table cells are single-line: fold breaks into a space.
            MdEvent::SoftBreak | MdEvent::HardBreak if self.in_table => self.push_text(" "),
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

    /// Render TeX math as Unicode, italic so it reads as math rather than prose.
    /// Display math (`$$…$$`) gets its own indented line; inside a table cell
    /// both forms stay inline so the grid stays single-line per cell.
    fn push_math(&mut self, tex: &str, display: bool) {
        let rendered = latex_to_unicode(tex);
        let style = code().add_modifier(Modifier::ITALIC);
        if display && !self.in_table {
            self.flush();
            self.line_prefix();
            self.current.push(Span::raw("  "));
            self.current.push(Span::styled(rendered, style));
            self.end_line();
            self.blank_line();
        } else {
            self.current.push(Span::styled(rendered, style));
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
            Tag::Table(aligns) => {
                self.flush();
                self.in_table = true;
                self.table_aligns = aligns;
            }
            Tag::TableHead => self.bold += 1,
            Tag::Emphasis => self.italic += 1,
            Tag::Strong => self.bold += 1,
            Tag::Strikethrough => self.strike += 1,
            Tag::Link { dest_url, .. } => self.link = Some(dest_url.to_string()),
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph if self.in_table => {}
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
            TagEnd::TableCell => {
                let cell = std::mem::take(&mut self.current);
                self.table_row.push(cell);
            }
            TagEnd::TableHead => {
                self.bold = self.bold.saturating_sub(1);
                self.table_header = true;
                self.table_rows.push(std::mem::take(&mut self.table_row));
            }
            TagEnd::TableRow => self.table_rows.push(std::mem::take(&mut self.table_row)),
            TagEnd::Table => self.end_table(),
            TagEnd::Emphasis => self.italic = self.italic.saturating_sub(1),
            TagEnd::Strong => self.bold = self.bold.saturating_sub(1),
            TagEnd::Strikethrough => self.strike = self.strike.saturating_sub(1),
            TagEnd::Link => {
                if let Some(url) = self.link.take() {
                    self.current.push(Span::styled(
                        format!(" ({url})"),
                        theme::style(Token::Link).underlined(),
                    ));
                }
            }
            _ => {}
        }
    }

    /// Emit the buffered table as an aligned grid: cells padded to their
    /// column's display width, dim `│` rules between columns, a dim `─┼─`
    /// rule after the header. Rows may be ragged (mid-stream truncation);
    /// missing cells pad as empty.
    ///
    /// When [`Self::table_width`] is finite and smaller than the natural
    /// grid, columns shrink toward content (floor 1) and long cells wrap
    /// inside their column so the row stays a coherent multi-line block
    /// instead of soft-wrapping mid-`│`.
    fn end_table(&mut self) {
        self.in_table = false;
        let has_header = std::mem::take(&mut self.table_header);
        let aligns = std::mem::take(&mut self.table_aligns);
        let rows = std::mem::take(&mut self.table_rows);
        let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
        if cols == 0 {
            return;
        }
        let mut widths = vec![0usize; cols];
        for row in &rows {
            for (col, cell) in row.iter().enumerate() {
                widths[col] = widths[col].max(spans_width(cell));
            }
        }
        // Shrink columns to fit the available width. Separators cost
        // 3 columns each (` │ `); leave at least one column of content
        // per cell so the grid never collapses to pure rules.
        let sep = 3usize;
        let seps = cols.saturating_sub(1).saturating_mul(sep);
        if self.table_width < usize::MAX {
            let budget = self.table_width.saturating_sub(seps);
            let natural: usize = widths.iter().sum();
            if natural > budget {
                fit_column_widths(&mut widths, budget);
            }
        }
        for (index, mut row) in rows.into_iter().enumerate() {
            row.resize_with(cols, Vec::new);
            // Each cell may wrap into several lines; the row height is the
            // tallest cell. Empty cells contribute a single blank line.
            let wrapped: Vec<Vec<CellSpans>> = row
                .into_iter()
                .enumerate()
                .map(|(col, cell)| wrap_cell(cell, widths[col].max(1)))
                .collect();
            let height = wrapped.iter().map(Vec::len).max().unwrap_or(1).max(1);
            for line_idx in 0..height {
                let mut spans = Vec::new();
                for (col, cell_lines) in wrapped.iter().enumerate() {
                    if col > 0 {
                        spans.push(Span::styled(" │ ", dim()));
                    }
                    let cell = cell_lines.get(line_idx).cloned().unwrap_or_default();
                    let pad = widths[col].saturating_sub(spans_width(&cell));
                    // Align only the first wrapped line of a multi-line cell;
                    // continuation lines stay left so the text column reads
                    // as a hanging block rather than a re-centered mess.
                    let (left, right) = if line_idx == 0 {
                        match aligns.get(col) {
                            Some(MdAlignment::Right) => (pad, 0),
                            Some(MdAlignment::Center) => (pad / 2, pad - pad / 2),
                            _ => (0, pad),
                        }
                    } else {
                        (0, pad)
                    };
                    if left > 0 {
                        spans.push(Span::raw(" ".repeat(left)));
                    }
                    spans.extend(cell);
                    if right > 0 {
                        spans.push(Span::raw(" ".repeat(right)));
                    }
                }
                self.lines.push(Line::from(spans));
            }
            if index == 0 && has_header {
                let rule = widths
                    .iter()
                    .map(|width| "─".repeat(*width))
                    .collect::<Vec<_>>()
                    .join("─┼─");
                self.lines.push(Line::from(Span::styled(rule, dim())));
            }
        }
        self.blank_line();
    }

    fn finish(mut self) -> Text<'static> {
        self.flush();
        while matches!(self.lines.last(), Some(line) if line.spans.is_empty()) {
            self.lines.pop();
        }
        Text::from(self.lines)
    }
}

/// Convert a TeX math fragment to a readable Unicode string for the TUI.
///
/// Terminals can't run KaTeX. We strip font-selection wrappers (`\mathrm`,
/// `\mathbf`, …), map a few blackboard/script sets unicodeit misses when they
/// sit next to nested braces, then run `unicodeit` for the bulk of symbols,
/// super/subscripts, and operators. Anything still untranslatable is left
/// as-is so the formula stays copy-pastable.
fn latex_to_unicode(tex: &str) -> String {
    let trimmed = tex.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let mut s = trimmed.to_string();
    // Drop pure-style wrappers so their contents become ordinary text that
    // unicodeit can subscript (`z_{\mathrm{pos}}` → `z_{pos}` → `zₚₒₛ`).
    for cmd in [
        "mathrm",
        "mathbf",
        "boldsymbol",
        "mathit",
        "mathsf",
        "mathtt",
        "operatorname",
        "text",
        "textrm",
        "textit",
        "textbf",
        "textsf",
        "texttt",
        "bm",
        "mathbfit",
    ] {
        s = strip_latex_cmd(&s, cmd);
    }
    // Blackboard / fraktur / script single-letter sets unicodeit only maps as
    // whole commands when the argument is a bare letter — expand them first.
    s = expand_letter_set(&s, "mathbb", MATHBB);
    s = expand_letter_set(&s, "mathcal", MATHCAL);
    s = expand_letter_set(&s, "mathscr", MATHCAL);
    s = expand_letter_set(&s, "mathfrak", MATHFRAK);
    // Common aliases. Longest first so `\rightarrow` isn't half-eaten by `\to`.
    // Only replace when the match is a whole TeX control word (next char is not
    // an ASCII letter) — otherwise `\in` would nibble `\infty`/`\int`/…
    let mut aliases = [
        ("\\Leftrightarrow", "⇔"),
        ("\\leftrightarrow", "↔"),
        ("\\Rightarrow", "⇒"),
        ("\\Leftarrow", "⇐"),
        ("\\rightarrow", "→"),
        ("\\leftarrow", "←"),
        ("\\subseteq", "⊆"),
        ("\\supseteq", "⊇"),
        ("\\subset", "⊂"),
        ("\\supset", "⊃"),
        ("\\emptyset", "∅"),
        ("\\partial", "∂"),
        ("\\nabla", "∇"),
        ("\\forall", "∀"),
        ("\\exists", "∃"),
        ("\\notin", "∉"),
        ("\\approx", "≈"),
        ("\\simeq", "≃"),
        ("\\equiv", "≡"),
        ("\\cong", "≅"),
        ("\\parallel", "∥"),
        ("\\perp", "⊥"),
        ("\\otimes", "⊗"),
        ("\\oplus", "⊕"),
        ("\\ominus", "⊖"),
        ("\\oslash", "⊘"),
        ("\\cdots", "⋯"),
        ("\\vdots", "⋮"),
        ("\\ddots", "⋱"),
        ("\\ldots", "…"),
        ("\\times", "×"),
        ("\\cdot", "·"),
        ("\\bullet", "•"),
        ("\\circ", "∘"),
        ("\\ast", "∗"),
        ("\\wedge", "∧"),
        ("\\vee", "∨"),
        ("\\mid", "∣"),
        ("\\sim", "∼"),
        ("\\neq", "≠"),
        ("\\leq", "≤"),
        ("\\geq", "≥"),
        ("\\ll", "≪"),
        ("\\gg", "≫"),
        ("\\in", "∈"),
        ("\\ni", "∋"),
        ("\\cup", "∪"),
        ("\\cap", "∩"),
        ("\\neg", "¬"),
        ("\\top", "⊤"),
        ("\\bot", "⊥"),
        ("\\infty", "∞"),
        ("\\pm", "±"),
        ("\\mp", "∓"),
        ("\\ne", "≠"),
        ("\\le", "≤"),
        ("\\ge", "≥"),
        ("\\to", "→"),
    ];
    aliases.sort_by_key(|(from, _)| std::cmp::Reverse(from.len()));
    for (from, to) in aliases {
        s = replace_tex_cmd(&s, from, to);
    }
    // `^\top` → superscript T (transpose) rather than a caret + ⊤.
    s = s.replace("^⊤", "ᵀ").replace("^{⊤}", "ᵀ");
    // \frac{a}{b} → (a)/(b)  (unicodeit leaves \frac alone)
    s = rewrite_frac(&s);
    // \sqrt{x} → √(x)
    s = rewrite_sqrt(&s);
    let out = unicodeit::replace(&s);
    // Collapse leftover empty braces and double spaces unicodeit can leave.
    // Flatten unconverted `_{word}` / `^{word}` to `_word` / `^word` so mixed
    // scripts (e.g. `zₚₒₛ` next to a non-subscriptable `neg`) stay readable.
    let out = flatten_unconverted_scripts(&out);
    let out = out.replace("{}", "").replace("  ", " ");
    out.trim().to_string()
}

/// Turn leftover `_{abc}` / `^{abc}` into `_abc` / `^abc` (braces only gone when
/// unicodeit couldn't map every character to a dedicated super/subscript).
fn flatten_unconverted_scripts(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        let ch = bytes[i] as char;
        if (ch == '_' || ch == '^')
            && i + 1 < bytes.len()
            && bytes[i + 1] == b'{'
            && let Some((body, after)) = take_balanced(&input[i + 2..])
        {
            out.push(ch);
            out.push_str(body);
            // `after` is a suffix of `input`; advance `i` by the matched span.
            i = input.len() - after.len();
            continue;
        }
        // Copy one UTF-8 char.
        let next = input[i..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
        out.push_str(&input[i..i + next]);
        i += next;
    }
    out
}

/// Blackboard-bold capitals (`\mathbb{R}` → ℝ).
const MATHBB: &[(char, char)] = &[
    ('A', '𝔸'),
    ('B', '𝔹'),
    ('C', 'ℂ'),
    ('D', '𝔻'),
    ('E', '𝔼'),
    ('F', '𝔽'),
    ('G', '𝔾'),
    ('H', 'ℍ'),
    ('I', '𝕀'),
    ('J', '𝕁'),
    ('K', '𝕂'),
    ('L', '𝕃'),
    ('M', '𝕄'),
    ('N', 'ℕ'),
    ('O', '𝕆'),
    ('P', 'ℙ'),
    ('Q', 'ℚ'),
    ('R', 'ℝ'),
    ('S', '𝕊'),
    ('T', '𝕋'),
    ('U', '𝕌'),
    ('V', '𝕍'),
    ('W', '𝕎'),
    ('X', '𝕏'),
    ('Y', '𝕐'),
    ('Z', 'ℤ'),
];

/// Calligraphic / script capitals (`\mathcal{L}` → ℒ).
const MATHCAL: &[(char, char)] = &[
    ('A', '𝒜'),
    ('B', 'ℬ'),
    ('C', '𝒞'),
    ('D', '𝒟'),
    ('E', 'ℰ'),
    ('F', 'ℱ'),
    ('G', '𝒢'),
    ('H', 'ℋ'),
    ('I', 'ℐ'),
    ('J', '𝒥'),
    ('K', '𝒦'),
    ('L', 'ℒ'),
    ('M', 'ℳ'),
    ('N', '𝒩'),
    ('O', '𝒪'),
    ('P', '𝒫'),
    ('Q', '𝒬'),
    ('R', 'ℛ'),
    ('S', '𝒮'),
    ('T', '𝒯'),
    ('U', '𝒰'),
    ('V', '𝒱'),
    ('W', '𝒲'),
    ('X', '𝒳'),
    ('Y', '𝒴'),
    ('Z', '𝒵'),
];

/// Fraktur capitals (`\mathfrak{g}` keeps lowercase as-is via the map).
const MATHFRAK: &[(char, char)] = &[
    ('A', '𝔄'),
    ('B', '𝔅'),
    ('C', 'ℭ'),
    ('D', '𝔇'),
    ('E', '𝔈'),
    ('F', '𝔉'),
    ('G', '𝔊'),
    ('H', 'ℌ'),
    ('I', 'ℑ'),
    ('J', '𝔍'),
    ('K', '𝔎'),
    ('L', '𝔏'),
    ('M', '𝔐'),
    ('N', '𝔑'),
    ('O', '𝔒'),
    ('P', '𝔓'),
    ('Q', '𝔔'),
    ('R', 'ℜ'),
    ('S', '𝔖'),
    ('T', '𝔗'),
    ('U', '𝔘'),
    ('V', '𝔙'),
    ('W', '𝔚'),
    ('X', '𝔛'),
    ('Y', '𝔜'),
    ('Z', 'ℨ'),
];

/// Replace every `\cmd{…}` with its brace body (style wrappers only).
fn strip_latex_cmd(input: &str, cmd: &str) -> String {
    let needle = format!("\\{cmd}{{");
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(at) = rest.find(&needle) {
        out.push_str(&rest[..at]);
        rest = &rest[at + needle.len()..];
        match take_balanced(rest) {
            Some((body, after)) => {
                out.push_str(body);
                rest = after;
            }
            None => {
                // Unbalanced — keep the command literal and stop scanning.
                out.push_str(&needle);
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Replace `\cmd{X}` for single-letter `X` via `map`; multi-letter bodies stay.
fn expand_letter_set(input: &str, cmd: &str, map: &[(char, char)]) -> String {
    let needle = format!("\\{cmd}{{");
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(at) = rest.find(&needle) {
        out.push_str(&rest[..at]);
        rest = &rest[at + needle.len()..];
        match take_balanced(rest) {
            Some((body, after)) => {
                let mut chars = body.chars();
                if let (Some(c), None) = (chars.next(), chars.next()) {
                    if let Some((_, uni)) = map.iter().find(|(k, _)| *k == c) {
                        out.push(*uni);
                    } else {
                        out.push(c);
                    }
                } else {
                    // Multi-char body: keep original command so nothing is lost.
                    out.push_str(&needle);
                    out.push_str(body);
                    out.push('}');
                }
                rest = after;
            }
            None => {
                out.push_str(&needle);
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Rewrite `\frac{num}{den}` → `(num)/(den)`.
fn rewrite_frac(input: &str) -> String {
    let needle = "\\frac{";
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(at) = rest.find(needle) {
        out.push_str(&rest[..at]);
        rest = &rest[at + needle.len()..];
        let Some((num, after_num)) = take_balanced(rest) else {
            out.push_str(needle);
            break;
        };
        rest = after_num;
        if !rest.starts_with('{') {
            out.push_str("\\frac{");
            out.push_str(num);
            out.push('}');
            continue;
        }
        rest = &rest[1..];
        let Some((den, after_den)) = take_balanced(rest) else {
            out.push_str("\\frac{");
            out.push_str(num);
            out.push_str("}{");
            break;
        };
        // Parenthesize only when the side has operators / multiple tokens.
        out.push_str(&paren_if_needed(num));
        out.push('/');
        out.push_str(&paren_if_needed(den));
        rest = after_den;
    }
    out.push_str(rest);
    out
}

/// Rewrite `\sqrt{x}` → `√(x)` and `\sqrt[n]{x}` → `ⁿ√(x)`.
fn rewrite_sqrt(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(at) = rest.find("\\sqrt") {
        out.push_str(&rest[..at]);
        rest = &rest[at + "\\sqrt".len()..];
        let mut index = String::new();
        if rest.starts_with('[')
            && let Some(end) = rest.find(']')
        {
            index.push_str(&rest[1..end]);
            rest = &rest[end + 1..];
        }
        if rest.starts_with('{') {
            rest = &rest[1..];
            if let Some((body, after)) = take_balanced(rest) {
                if !index.is_empty() {
                    // Map simple digit indices to superscripts when possible.
                    for ch in index.chars() {
                        out.push(match ch {
                            '0' => '⁰',
                            '1' => '¹',
                            '2' => '²',
                            '3' => '³',
                            '4' => '⁴',
                            '5' => '⁵',
                            '6' => '⁶',
                            '7' => '⁷',
                            '8' => '⁸',
                            '9' => '⁹',
                            _ => ch,
                        });
                    }
                }
                out.push('√');
                out.push_str(&paren_if_needed(body));
                rest = after;
                continue;
            }
        }
        // Fallback: leave the command text.
        out.push_str("\\sqrt");
        if !index.is_empty() {
            out.push('[');
            out.push_str(&index);
            out.push(']');
        }
    }
    out.push_str(rest);
    out
}

fn paren_if_needed(body: &str) -> String {
    let t = body.trim();
    if t.is_empty() {
        return "()".to_string();
    }
    // Already parenthesized, or a single atom (letter/digit/symbol run).
    if (t.starts_with('(') && t.ends_with(')'))
        || t.chars().count() == 1
        || t.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return t.to_string();
    }
    format!("({t})")
}

/// Replace every occurrence of a TeX control sequence `cmd` (e.g. `\in`) with
/// `with`, but only at a control-word boundary — the character after the match
/// must not be an ASCII letter. Stops `\in` from eating `\infty`/`\int`.
fn replace_tex_cmd(input: &str, cmd: &str, with: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(at) = rest.find(cmd) {
        let after_cmd = &rest[at + cmd.len()..];
        let next_is_letter = after_cmd
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic());
        if next_is_letter {
            // Not a whole command — copy through the match and keep scanning.
            out.push_str(&rest[..at + cmd.len()]);
            rest = after_cmd;
            continue;
        }
        out.push_str(&rest[..at]);
        out.push_str(with);
        rest = after_cmd;
    }
    out.push_str(rest);
    out
}

/// Given input starting *after* an opening `{`, return `(body, rest_after_close)`.
fn take_balanced(input: &str) -> Option<(&str, &str)> {
    let mut depth = 1usize;
    for (i, ch) in input.char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some((&input[..i], &input[i + 1..]));
                }
            }
            _ => {}
        }
    }
    None
}

fn spans_width(spans: &[Span]) -> usize {
    spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum()
}

/// Shrink `widths` in place so their sum is at most `budget`, never below 1
/// per column. Distributes the deficit by repeatedly taking one column from
/// the current widest — keeps relative importance, avoids starving a short
/// column while a long one still has room.
fn fit_column_widths(widths: &mut [usize], budget: usize) {
    if widths.is_empty() {
        return;
    }
    let floor = 1usize;
    let min_total = floor.saturating_mul(widths.len());
    let target = budget.max(min_total);
    // Cap anything already below floor first so the loop can't go negative.
    for w in widths.iter_mut() {
        *w = (*w).max(floor);
    }
    let mut total: usize = widths.iter().sum();
    while total > target {
        // Prefer trimming the current widest column that is still above floor.
        let Some((idx, _)) = widths
            .iter()
            .enumerate()
            .filter(|(_, w)| **w > floor)
            .max_by_key(|(_, w)| *w)
        else {
            break;
        };
        widths[idx] -= 1;
        total -= 1;
    }
}

/// Word-wrap a single table cell into lines of at most `width` display
/// columns, preserving span styles. Falls back to hard char splits only
/// when one word exceeds the budget. A thin adapter over [`LineWrapper`]
/// (the transcript word-wrap core) with no hanging indent.
fn wrap_cell(cell: CellSpans, width: usize) -> Vec<CellSpans> {
    let width = width.max(1);
    if spans_width(&cell) <= width {
        return vec![cell];
    }
    let mut wrapper = LineWrapper::new(width, 0, &Line::default());
    for span in &cell {
        for ch in span.content.chars() {
            wrapper.feed(ch, span.style);
        }
    }
    wrapper
        .finish()
        .into_iter()
        .map(|line| line.spans)
        .collect()
}

#[cfg(test)]
mod tests;
