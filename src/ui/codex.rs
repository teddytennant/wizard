//! Codex's chrome: Wizard wearing the OpenAI Codex CLI's terminal UI.
//!
//! This module owns the *whole* frame under `[ui] skin = "codex"`, not a set of
//! glyph substitutions. Codex's screen is laid out differently enough from the
//! house one that sharing a layout and swapping markers cannot reach it:
//!
//! - There is **no bottom status bar**. A turn in flight narrates itself on its
//!   own row *above* the composer (`bottom_pane/mod.rs:1774-1809`), a bare blank
//!   line separates that from the composer, and the only thing under the
//!   composer is one row of key hints — `FOOTER_SPACING_HEIGHT == 0`
//!   (`bottom_pane/chat_composer.rs:539`), so there is not even a gap.
//! - The composer has **no frame at all**: no border, no rules. A `›` hangs in
//!   the margin at column 0, the draft starts at column 2, one column is held
//!   back at the right, and the entire rect is painted with the same background
//!   a committed user message gets (`chat_composer.rs:936-990`, `:4751-4752`).
//! - Everything hangs off two-column gutters: `• ` for the agent, `› ` for the
//!   user, `  └ ` / `    ` for a tool's output, `  │ ` for a wrapped command.
//!
//! Every constant and algorithm below that came from upstream carries the file
//! it came from. Codex is Apache-2.0 (see `LICENSE`/`NOTICE` at
//! <https://github.com/openai/codex>); Wizard is MIT. `docs/ui-skins.md` holds
//! the overall attribution, and [`crate::skin::blend`] /
//! [`crate::skin::motion`] hold the two ports that are shared with `grok`.
//!
//! ## This is Wizard, wearing Codex
//!
//! The chrome is borrowed; the content is not. The commands are Wizard's
//! (`/model`, `/ui`, `/theme`, `/fusion`, `/ultra`, `/publish`, `/help`), and
//! every piece of Wizard's own state stays on screen. Nothing is dropped to
//! look more like Codex; each one is drawn as the Codex cell that means the
//! same thing — and, just as strictly, nothing is *added* to a cell that was
//! ported whole. A count appended to a header or a hint tacked onto a row is
//! how a port stops reading as the thing and starts reading as a copy of it,
//! so state that has no home in a borrowed cell goes to the surface that
//! already carries its kind of fact (the footer's status line, the key list):
//!
//! | Wizard | Codex cell it wears |
//! |---|---|
//! | subagents, the rail | the collaboration cells, `multi_agents.rs:460-530` |
//! | background tasks (`/bashes`) | background-terminal cells, `history_cell/exec.rs:21-64` |
//! | the todo list (`/todos`) | the plan cells, `history_cell/plans.rs:175-226` |
//! | the interview modal | `history_cell/request_user_input.rs:28-105` |
//! | the slash-command popup | `bottom_pane/command_popup.rs` on a menu surface |
//! | `/model`, `/mode`, settings | `bottom_pane/list_selection_view.rs` |
//! | plan review | the approval overlay, `bottom_pane/approval_overlay.rs` |
//! | MCP and every other tool | `history_cell/mcp.rs`'s `Called tool(args)` |
//! | notices | `history_cell/notices.rs` (`• `, `■ `, `⚠ `) |
//! | plan / omakase mode | the `Plan mode` label and its `shift+tab` cycle hint |
//! | context meter, ultra, fusion, detached work | the footer's right-hand context |
//! | queued messages | the status row's inline message and the queue hint |
//! | `/help` and the key list | the `ShortcutOverlay` footer mode |
//! | the welcome screen | the session header card + first-run help block |
//!
//! Two things keep a shared renderer, and only because Codex has nothing of
//! the kind: the `/diff` sidebar (Codex renders a diff as a patch *cell*, which
//! this module does draw — the sidebar is a second, scrollable view Wizard adds
//! on top) and `/dashboard`, a machine-wide session manager.
//!
//! ## Where the house rules beat fidelity
//!
//! Two of the design rules at the top of [`super`] are load-bearing and win
//! wherever they collide with what Codex does. Both collisions are marked
//! `HOUSE RULE` at the site:
//!
//! 1. **Meaning never rests on hue.** Codex distinguishes a failed exec from a
//!    successful one by painting the same `•` red instead of green
//!    (`exec_cell/render.rs:357-364`). Under 16 colors, `NO_COLOR`, or the
//!    `minimal` palette that is no distinction at all, so failure keeps a `✗`.
//! 2. **Tokens, never colors.** Nothing here names a `ratatui::style::Color`;
//!    it asks [`crate::theme`] for a token and `assets/themes/codex.toml` maps
//!    those onto Codex's palette. The one `Color` *type* mentioned below is the
//!    return of [`Tint::resolve`], which is the sanctioned exception: a slab is
//!    an alpha over the terminal's real background when that is knowable, and
//!    the theme's `bg.raised` when it is not.
//!
//! A third divergence is honesty about keys. Codex's elision line names
//! `ctrl + t to view transcript` and its status row names `esc to interrupt`;
//! in Wizard those keys are `ctrl + t` (toggles the last tool card) and
//! `ctrl + c`. A hint that names a key which does something else is worse than
//! a hint in the wrong font, so the wording follows the key.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Paragraph};
use unicode_width::UnicodeWidthStr;

use crate::app::{App, InputMode, PaneStatus};
use crate::config::Mode;
use crate::image_view::{ImageBlock, ImageBox, ImageCache};
use crate::skin::blend::{Tint, terminal_bg};
use crate::skin::{self, motion};
use crate::theme::{self, ColorDepth, Token};
use crate::transcript::{ToolItem, TranscriptItem};

use super::{RowTag, accent, dim, muted, warning};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Columns the prompt gutter reserves, and therefore the indent everything
/// aligned to it uses. `codex-rs/tui/src/ui_consts.rs:10`.
const LIVE_PREFIX_COLS: u16 = 2;

/// The footer's hard left indent. `ui_consts.rs:11` (`== LIVE_PREFIX_COLS`).
const FOOTER_INDENT_COLS: u16 = LIVE_PREFIX_COLS;

/// Minimum columns between the footer's left text and its right-hand context.
/// `bottom_pane/footer.rs:106`.
const FOOTER_CONTEXT_GAP_COLS: u16 = 1;

/// Rows a tool's output may occupy before it is elided from the middle.
/// `exec_cell/render.rs:33` (`TOOL_CALL_MAX_LINES`).
const TOOL_CALL_MAX_LINES: usize = 5;

/// Rows a wrapped command may spill onto under its header.
/// `exec_cell/render.rs:678` (`command_continuation_max_lines`).
const COMMAND_CONTINUATION_MAX_LINES: usize = 2;

/// The output block's gutter: the arm on its first row, blanks under it.
/// `exec_cell/render.rs:679`. Both are four columns, which is what
/// `PrefixedBlock::wrap_width` (`render.rs:664-668`) subtracts *before*
/// prefixing — see [`prefixed_block`].
const OUTPUT_ARM: (&str, &str) = ("  └ ", "    ");

/// The wrapped-command gutter. `exec_cell/render.rs:676`.
const COMMAND_ARM: (&str, &str) = ("  │ ", "  │ ");

/// Rows the status widget's details may fill.
/// `status_indicator_widget.rs:35`.
const STATUS_DETAILS_MAX_LINES: usize = 3;

/// The status details gutter. `status_indicator_widget.rs:36`.
const DETAILS_ARM: (&str, &str) = ("  └ ", "    ");

/// Widest the session-header card's contents may get.
/// `history_cell/session.rs:8`.
const SESSION_HEADER_MAX_INNER_WIDTH: usize = 56;

/// The mode-cycle hint, spelled without spaces around the `+` exactly as
/// upstream does (`bottom_pane/footer.rs:105`) — key *labels* elsewhere are
/// `ctrl + c`, this one is not.
const MODE_CYCLE_HINT: &str = "shift+tab to cycle";

/// What an empty composer says. Codex's is "Ask Codex to do anything"
/// (`chatwidget.rs:1979`); this is the same sentence about a different agent.
const PLACEHOLDER: &str = "Ask Wizard to do anything";

/// Codex names `ctrl + t to view transcript` here (`ui_consts.rs:12`). Wizard's
/// Ctrl-T toggles the last tool card, which *is* the elided output, so the hint
/// names what the key actually does.
const EXPAND_HINT: &str = "ctrl + t to expand";

/// Tallest the plan band grows before it scrolls to the current step.
const MAX_PLAN_ROWS: u16 = 10;

/// Rail rows drawn at once, matching the house rail's cap so the two agree
/// about when "+N more" appears.
const MAX_RAIL_ROWS: usize = 5;

// ---------------------------------------------------------------------------
// Small style helpers
// ---------------------------------------------------------------------------

/// Body text at the terminal's own foreground.
fn text_style() -> Style {
    theme::style(Token::Text)
}

/// The slab behind a user message, a proposed plan, a menu surface and the
/// composer: `blend(white, terminal_bg, 0.12)` on a dark terminal,
/// `blend(black, terminal_bg, 0.04)` on a light one.
///
/// Ported from `codex-rs/tui/src/style.rs:74-91` by way of
/// [`Tint::resolve`], which owns the arithmetic *and* the fallback: Codex can
/// ask the terminal for its background and Wizard deliberately does not (an
/// escape query that blocks on a reply hangs the TUI at startup), so when the
/// environment does not report one the theme's `bg.raised` stands in. Only a
/// theme that declares `reset` — which is what the two house palettes do —
/// paints nothing.
fn slab_bg() -> Option<Color> {
    Tint::Raised.resolve()
}

/// Paint a floating layer's background and return the rect its content goes
/// in.
///
/// `bottom_pane/selection_popup_common.rs:91-119`: every selection-style
/// overlay Codex has — `/model`, approvals, request-user-input, the slash
/// popup — is a *menu surface*, which is the same slab a user message sits on
/// inset by one row and two columns. There is no border anywhere in it. That
/// is the whole reason this skin has no box-drawing characters outside the
/// session-header card.
fn menu_surface(frame: &mut Frame, area: Rect) -> Rect {
    if area.is_empty() {
        return area;
    }
    // `Clear` first: these float over the transcript, and a slab that is only
    // *mostly* opaque would let the text under it read through.
    frame.render_widget(ratatui::widgets::Clear, area);
    if let Some(color) = slab_bg() {
        frame.render_widget(Block::default().style(Style::default().bg(color)), area);
    }
    Rect {
        x: area.x + MENU_SURFACE_INSET_H,
        y: area.y + MENU_SURFACE_INSET_V,
        width: area.width.saturating_sub(MENU_SURFACE_INSET_H * 2),
        height: area.height.saturating_sub(MENU_SURFACE_INSET_V * 2),
    }
}

/// Rows and columns a menu surface insets its content by.
/// `selection_popup_common.rs:91-92`.
const MENU_SURFACE_INSET_V: u16 = 1;
const MENU_SURFACE_INSET_H: u16 = 2;

/// One row of a selection list.
///
/// `selection_popup_common.rs:455-530`: there is **no marker column**. The
/// selected row is recognized by having every one of its spans restyled to the
/// accent (`apply_row_state_style`, `:334-350`), which is what makes a Codex
/// menu read as a menu without a `❯` down its left edge. `label` is padded to
/// the description column so the right-hand blurbs line up.
fn menu_row(
    label: String,
    detail: &str,
    selected: bool,
    label_col: usize,
    width: usize,
) -> Line<'static> {
    let mut spans = vec![Span::styled(
        format!("{label:<label_col$}"),
        if selected {
            accent().bold()
        } else {
            text_style()
        },
    )];
    if !detail.is_empty() {
        let room = width.saturating_sub(label_col.max(label.width()) + 2);
        if room >= 4 {
            spans.push(Span::styled(
                format!("  {}", super::truncate_width(detail, room)),
                if selected { accent() } else { dim() },
            ));
        }
    }
    super::truncate_line(Line::from(spans), width)
}

/// `style` with the slab background applied, when there is one.
fn on_slab(style: Style, bg: Option<Color>) -> Style {
    match bg {
        Some(color) => style.bg(color),
        None => style,
    }
}

/// Wrap `rows` (already the right width) in a gutter: `arm.0` on the first row,
/// `arm.1` on every row below it.
///
/// This is the wrap-then-prefix order — `render::line_utils::prefix_lines`
/// (`codex-rs/tui/src/render/line_utils.rs:57-76`) — and the caller must have
/// pre-shrunk the wrap width itself. [`prefixed_block`] is the pair of helpers
/// that keeps the two numbers together.
fn arm(rows: Vec<Line<'static>>, arm: (&'static str, &'static str)) -> Vec<Line<'static>> {
    super::prefix_rows(rows, arm.0, arm.1, dim())
}

/// The width a block prefixed with `arm` must wrap to: the total minus the
/// wider of the two prefixes, floored at 1.
///
/// Ported from `PrefixedBlock::wrap_width`, `exec_cell/render.rs:664-668`. Both
/// halves of an arm are the same width by construction (`"  └ "` and `"    "`
/// are four columns each), and the `max` is what guarantees the gutter can
/// never push content past the right edge even though it is added *after* the
/// wrap.
fn prefixed_block(width: usize, arm: (&'static str, &'static str)) -> usize {
    width
        .saturating_sub(arm.0.width().max(arm.1.width()))
        .max(1)
}

/// Pad `spans` out to `width` columns with the slab's background, so a tinted
/// block is a rectangle rather than the ragged shape of its text.
fn slab_line(mut spans: Vec<Span<'static>>, width: usize, bg: Option<Color>) -> Line<'static> {
    if bg.is_some() {
        let used: usize = spans.iter().map(|span| span.content.width()).sum();
        if used < width {
            spans.push(Span::styled(
                " ".repeat(width - used),
                on_slab(text_style(), bg),
            ));
        }
    }
    Line::from(spans)
}

// ---------------------------------------------------------------------------
// Frame layout
// ---------------------------------------------------------------------------

/// The rows a Codex frame is laid out into, top to bottom.
///
/// The shape is upstream's bottom pane read literally: a flex group holding the
/// status widget, then a bare `""` separator line pushed when there *is* a
/// status above the composer (`bottom_pane/mod.rs:1782-1796`), then the
/// composer, then the footer with no spacing between them at all.
#[derive(Debug, Clone, Copy)]
struct Regions {
    /// The transcript.
    body: Rect,
    /// Wizard's todo list, drawn as a Codex plan cell.
    plan: Rect,
    /// The turn's status row, plus its details lines. Zero rows when idle.
    ///
    /// A bare blank line always follows it (`bottom_pane/mod.rs:1782`); it is
    /// laid out but never drawn into, so it has no field of its own — the
    /// composer simply starts one row below the status group.
    status: Rect,
    /// The composer: three rows minimum, no frame.
    composer: Rect,
    /// Wizard's subagent rail, drawn as a Codex agent cell.
    rail: Rect,
    /// One row of key hints. Never a status bar.
    footer: Rect,
}

/// Lay `area` out.
fn regions(app: &App, area: Rect) -> Regions {
    let composer_rows = composer_height(app, area.width);
    let rail_rows = rail_height(app);
    let status_rows = status_height(app, area.width);
    // `bottom_pane/mod.rs:1782` pushes exactly one bare line between the status
    // group and the composer, and only when there is a status group.
    let gap_rows = u16::from(status_rows > 0);
    let footer_rows = footer_height(app);
    let plan_rows = plan_height(
        app,
        area.height,
        composer_rows + status_rows + gap_rows + footer_rows,
        rail_rows,
    );
    let [body, rail, plan, status, _gap, composer, footer] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(rail_rows),
        Constraint::Length(plan_rows),
        Constraint::Length(status_rows),
        Constraint::Length(gap_rows),
        Constraint::Length(composer_rows),
        Constraint::Length(footer_rows),
    ])
    .areas(area);
    Regions {
        body,
        plan,
        status,
        composer,
        rail,
        footer,
    }
}

/// Columns the draft may fill: the full width less the two-column prompt gutter
/// and the one-column right inset.
///
/// `chat_composer.rs:4441` calls this `COLS_WITH_MARGIN` and spells it
/// `LIVE_PREFIX_COLS + 1`. It is the same `width - 3` a committed user message
/// wraps to (`history_cell/messages.rs:117-121`), which is the whole point: a
/// submitted message lands in exactly the columns it was typed in.
fn composer_budget(width: u16) -> usize {
    (width as usize)
        .saturating_sub(LIVE_PREFIX_COLS as usize + 1)
        .max(1)
}

/// Rows the composer occupies: one blank inset above, the draft, one blank
/// inset below — never fewer than three (`Constraint::Min(3)`,
/// `chat_composer.rs:936`).
fn composer_height(app: &App, width: u16) -> u16 {
    let budget = composer_budget(width);
    let rows = super::wrap_rows(&super::composer_chars(app), budget).len() as u16;
    rows.clamp(1, super::MAX_INPUT_ROWS) + 2
}

/// Rows the status group occupies: the header row plus its details, or nothing
/// at all when no turn is in flight. `status_indicator_widget.rs:234-236`.
fn status_height(app: &App, width: u16) -> u16 {
    if !status_visible(app) {
        return 0;
    }
    1 + status_details(app, width).len() as u16
}

/// Is a turn (or a turn-shaped background job) running?
fn status_visible(app: &App) -> bool {
    app.status.busy || app.rebuilding.is_some() || app.compacting
}

/// Rows the plan band occupies. Zero when hidden; capped so a long list cannot
/// swallow the transcript, with the same floor the house band uses.
fn plan_height(app: &App, total: u16, bottom_rows: u16, rail_rows: u16) -> u16 {
    if !app.show_todos {
        return 0;
    }
    // Header, then one row per step (or one for the empty-plan placeholder).
    let desired = 1 + (app.todos.len() as u16).max(1);
    // Leave a transcript row, the bottom pane, the rail and the footer alone.
    let reserved = 1u16
        .saturating_add(bottom_rows)
        .saturating_add(rail_rows)
        .saturating_add(1);
    let available = total.saturating_sub(reserved);
    if available < 2 {
        return 0;
    }
    desired.min(available).min(MAX_PLAN_ROWS)
}

/// Rows the footer occupies: one, except while the shortcut overlay is up.
/// There is never any spacing above it — `FOOTER_SPACING_HEIGHT == 0`
/// (`chat_composer.rs:539`).
fn footer_height(app: &App) -> u16 {
    match FooterMode::of(app) {
        FooterMode::ShortcutOverlay => shortcut_overlay_lines(app).len() as u16,
        _ => 1,
    }
}

/// Rows the rail occupies: a header, one row per subagent (capped), and a
/// `+N more` marker when it is capped.
fn rail_height(app: &App) -> u16 {
    if app.panes.is_empty() {
        return 0;
    }
    let shown = app.panes.len().min(MAX_RAIL_ROWS);
    let overflow = usize::from(app.panes.len() > MAX_RAIL_ROWS);
    (1 + shown + overflow) as u16
}

/// Render one frame in Codex's chrome.
pub(super) fn draw(frame: &mut Frame, app: &App) {
    let Regions {
        body,
        plan,
        status,
        composer,
        rail,
        footer,
    } = regions(app, frame.area());

    if let Some(pane) = app.attached_pane() {
        // Inside a subagent: its conversation takes the whole body, drawn by
        // the shared pane renderer so an attached run looks like the chat it
        // came out of whichever skin is on.
        super::draw_pane(frame, app, pane, body);
    } else if app.diff.is_some() {
        let [chat, side] =
            Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)])
                .areas(body);
        draw_transcript(frame, app, chat);
        super::draw_diff_sidebar(frame, app, side);
    } else {
        draw_transcript(frame, app, body);
    }

    if plan.height > 0 {
        draw_plan_band(frame, app, plan);
    }
    if status.height > 0 {
        draw_status(frame, app, status);
    }
    draw_composer(frame, app, composer);
    if rail.height > 0 {
        draw_rail(frame, app, rail);
    }
    if footer.height > 0 {
        draw_footer(frame, app, footer);
    }

    // Floating layers, all on Codex's menu surface: a slab, an inset, and no
    // border anywhere. The one exception is the session dashboard, which is a
    // full-screen machine-wide session manager with nothing like it upstream
    // and which keeps the shared renderer.
    if !super::overlay_open(app) {
        draw_command_popup(frame, app, composer);
    }
    if app.picker.is_some() {
        draw_picker(frame, app, body);
    }
    if app.plan_review.is_some() {
        draw_plan_review(frame, app, body);
    }
    if app.interview.is_some() {
        draw_interview(frame, app, body);
    }
    if app.show_dashboard {
        super::draw_dashboard(frame, app);
    }

    // With an overlay floating over the transcript a click belongs to the
    // overlay, so no card underneath may claim it.
    if super::overlay_open(app) {
        app.card_hits.borrow_mut().clear();
    }

    // The drag-selection highlight paints last so it reverses whatever ended
    // up on screen.
    if let Some(selection) = app.selection {
        let area = frame.area();
        let buf = frame.buffer_mut();
        for (y, start, end) in super::selection_rows(&selection, area.width, area.height) {
            for x in start..end {
                if let Some(cell) = buf.cell_mut(Position::new(x, y)) {
                    cell.modifier.insert(Modifier::REVERSED);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The transcript
// ---------------------------------------------------------------------------

/// The transcript: Codex's history cells, scrolled, with image blocks painted
/// into the rows they reserved.
///
/// Drawn flush at column 0 with no side margin and **no scrollbar**: Codex
/// writes into the terminal's own scrollback and has neither. The `↓ N more`
/// hint stays, because without a scrollbar it is the only thing that says
/// there is more below.
fn draw_transcript(frame: &mut Frame, app: &App, area: Rect) {
    // Rebuilt every frame, cleared up front so the early returns cannot leave
    // stale clickable rows behind.
    app.card_hits.borrow_mut().clear();
    if area.width == 0 || area.height == 0 {
        return;
    }

    if app.welcome_visible() {
        if !super::overlay_open(app) {
            draw_session_header(frame, app, area);
        }
        return;
    }

    let width = area.width as usize;
    let height = area.height as usize;
    let mut cache = app.images.borrow_mut();
    let rendered = transcript_lines(app, &mut cache, super::image_box(area), width);
    let total = rendered.lines.len();
    let max_scroll = total.saturating_sub(height);
    app.transcript.max_scroll.set(max_scroll as u16);
    let start = if app.transcript.follow || max_scroll == 0 {
        max_scroll
    } else {
        (app.transcript.scroll as usize).min(max_scroll)
    };
    let remaining = max_scroll.saturating_sub(start);
    let end = (start + height).min(total);
    let visible: Vec<Line<'static>> = rendered.lines[start..end].to_vec();

    {
        let mut hits = app.card_hits.borrow_mut();
        for (offset, tag) in rendered.tags[start..end].iter().enumerate() {
            if let RowTag::Card(index) = tag {
                hits.push((area.y + offset as u16, *index));
            }
        }
    }

    let first_row_width = visible.first().map(|line| line.width() as u16).unwrap_or(0);
    frame.render_widget(Paragraph::new(Text::from(visible)), area);

    if !super::overlay_open(app) {
        super::paint_images(
            frame,
            area,
            &rendered.tags[start..end],
            &rendered.blocks,
            &mut cache,
        );
    }

    // The hint yields to the transcript when the two compete for the top row:
    // a decoration must never eat a word of what the user came to read.
    if remaining > 0 {
        let label = format!("↓ {remaining} more ");
        let label_width = (label.width() as u16).min(area.width);
        if first_row_width + label_width <= area.width {
            let hint = Rect {
                x: area.right().saturating_sub(label_width),
                y: area.y,
                width: label_width,
                height: 1,
            };
            frame.render_widget(Paragraph::new(Span::styled(label, dim())), hint);
        }
    }
}

/// A rendered transcript: the rows, what each row belongs to, and the image
/// blocks whose rows are waiting for pixels.
struct Cells {
    lines: Vec<Line<'static>>,
    tags: Vec<RowTag>,
    blocks: Vec<ImageBlock>,
}

/// Build every transcript row, committed items first and the uncommitted
/// streaming tail last.
///
/// The inter-cell blank line is upstream's rule exactly
/// (`app/resize_reflow.rs:91-100`): exactly one blank line is inserted *before*
/// every cell, except before the very first one emitted. Codex additionally
/// suppresses it for stream continuations (`history_cell/mod.rs:277`,
/// `messages.rs:349-351`); Wizard's model coalesces a stream into a single
/// item, so a continuation never reaches here as its own cell and the rule is
/// satisfied by construction.
fn transcript_lines(app: &App, cache: &mut ImageCache, budget: ImageBox, width: usize) -> Cells {
    let mut out = Cells {
        lines: Vec::new(),
        tags: Vec::new(),
        blocks: Vec::new(),
    };
    let mut first = true;
    let gap = |out: &mut Cells, first: &mut bool| {
        if *first {
            *first = false;
        } else {
            out.lines.push(Line::raw(""));
        }
    };

    for (index, item) in app.transcript.iter().enumerate() {
        // A turn boundary has no cell of its own: the transcript is one
        // continuous conversation and `/rewind` is what turns are for.
        if matches!(item, TranscriptItem::TurnMarker { .. }) {
            continue;
        }
        gap(&mut out, &mut first);
        let at = out.lines.len();
        match item {
            TranscriptItem::TurnMarker { .. } => {}
            TranscriptItem::User { text, .. } => out.lines.extend(user_cell(text, width)),
            TranscriptItem::Text(message) => out.lines.extend(agent_cell(message, width, false)),
            TranscriptItem::Thinking(message) => out.lines.extend(reasoning_cell(message, width)),
            TranscriptItem::Notice(message) => out.lines.extend(notice_cell(message, width)),
            TranscriptItem::Tool(tool) => {
                out.lines.extend(tool_cell(
                    tool,
                    app.transcript.folded(index),
                    app.tick,
                    width,
                ));
                out.tags.resize(out.lines.len(), RowTag::Text);
                // The header is the click-to-fold target.
                if at < out.lines.len() {
                    out.tags[at] = RowTag::Card(index);
                }
            }
            // Images reserve their rows as blanks and are painted into them
            // afterwards, so they scroll and clip like any other content.
            TranscriptItem::Images { source, images } => {
                for image in images {
                    if let Some(block) = cache.layout(image, budget) {
                        let rows = block.rows;
                        let slot = out.blocks.len();
                        let top = out.lines.len();
                        out.lines
                            .extend(std::iter::repeat_n(Line::raw(""), rows as usize));
                        out.blocks.push(block);
                        out.tags.resize(out.lines.len(), RowTag::Text);
                        for row in 0..rows {
                            out.tags[top + row as usize] = RowTag::Image { slot, row };
                        }
                    }
                    out.lines.extend(super::image_caption(source, image));
                }
            }
        }
        out.tags.resize(out.lines.len(), RowTag::Text);
    }

    // The uncommitted tail, decorated exactly like a committed cell so nothing
    // shifts sideways at the moment a turn lands.
    let (thinking, streaming) = app.transcript.streaming();
    if !thinking.is_empty() {
        gap(&mut out, &mut first);
        out.lines.extend(reasoning_cell(thinking, width));
    }
    if !streaming.is_empty() {
        gap(&mut out, &mut first);
        out.lines.extend(agent_cell(streaming, width, true));
    }
    out.tags.resize(out.lines.len(), RowTag::Text);
    out
}

/// The user's message: a background-painted slab with a blank row above and
/// below it, and a bold-and-dim `› ` leading the first line.
///
/// `history_cell/messages.rs:109-198`. Two details that look like accidents and
/// are not:
///
/// - the text wraps at `width - 3`, not `width - 2`, leaving a deliberate
///   one-column right margin that matches the composer's `right: 1` inset
///   (`messages.rs:117-121`);
/// - `›` carries `BOLD` and `DIM` *simultaneously*, both modifiers on one span
///   (`messages.rs:191`), which is why it reads as a quiet mark rather than a
///   loud one.
fn user_cell(text: &str, width: usize) -> Vec<Line<'static>> {
    let bg = slab_bg();
    let wrap_width = width.saturating_sub(LIVE_PREFIX_COLS as usize + 1).max(1);
    let body: Vec<Line<'static>> = super::wrap_all(
        text.lines()
            .map(|line| Line::from(Span::styled(line.to_string(), on_slab(text_style(), bg))))
            .collect(),
        wrap_width,
    );
    let mark = |first: bool| {
        let glyph = if first { "› " } else { "  " };
        let style = if first {
            on_slab(dim().bold(), bg)
        } else {
            on_slab(text_style(), bg)
        };
        Span::styled(glyph, style)
    };

    let mut lines = vec![slab_line(Vec::new(), width, bg)];
    for (index, line) in body.into_iter().enumerate() {
        let mut spans = vec![mark(index == 0)];
        spans.extend(
            line.spans
                .into_iter()
                .map(|span| Span::styled(span.content, on_slab(span.style, bg))),
        );
        lines.push(slab_line(spans, width, bg));
    }
    lines.push(slab_line(Vec::new(), width, bg));
    lines
}

/// An agent message: markdown re-rendered at `width - 2`, then a dim `• ` on
/// the first row and two spaces under it.
///
/// `history_cell/messages.rs:425-475` for the finalized form. The streaming
/// form (`messages.rs:314-352`) reaches the same columns by the other route —
/// `initial_indent("• ")` at the full width — so one code path serves both;
/// what differs is only that in-flight code blocks stay unhighlighted, which is
/// what keeps a per-frame re-render cheap.
fn agent_cell(message: &str, width: usize, streaming: bool) -> Vec<Line<'static>> {
    let content_width = width.saturating_sub(LIVE_PREFIX_COLS as usize).max(1);
    let mut text = if streaming {
        super::render_markdown_streaming(message, content_width)
    } else {
        super::render_markdown_at(message, content_width)
    };
    if streaming {
        // A soft cursor at the tail, so a paused stream reads as paused rather
        // than as finished.
        let tail = Span::styled("▍", dim());
        match text.lines.last_mut() {
            Some(last) => last.spans.push(tail),
            None => text.lines.push(Line::from(tail)),
        }
    }
    let rows = super::wrap_lines(text, content_width);
    // The bullet is dim only — never bold. `messages.rs:120`.
    super::prefix_rows(rows, "• ", "  ", dim())
}

/// Model reasoning: the same `• ` gutter, dim *and* italic throughout.
/// `history_cell/messages.rs:239-289`, where every span of the rendered
/// summary is patched with `Style::default().dim().italic()` (`:247`).
///
/// Codex hides reasoning that carries no `**bold**` header from the viewport
/// entirely, surfacing it only under Ctrl+T (`messages.rs:270-276`). Wizard
/// shows it: reasoning is a first-class part of what this agent is doing, and
/// Wizard has no second transcript view to hide it in.
fn reasoning_cell(message: &str, width: usize) -> Vec<Line<'static>> {
    let content_width = width.saturating_sub(LIVE_PREFIX_COLS as usize).max(1);
    let style = dim().italic();
    let rows = super::wrap_all(
        message
            .lines()
            .map(|line| Line::from(Span::styled(line.to_string(), style)))
            .collect(),
        content_width,
    );
    super::prefix_rows(rows, "• ", "  ", dim())
}

/// A notice. `history_cell/notices.rs` gives each severity its own glyph and
/// its own rule about what is styled:
///
/// | kind | first line | continuation |
/// |---|---|---|
/// | info | `"• "` dim + message | `"  "` |
/// | error | `"■ {message}"`, the *whole* string red, no dim bullet (`:213-219`) |
/// | warning | `"⚠ "` yellow + yellow body (`:84-86`) | `"  "` |
///
/// Wizard's notices are plain strings, so the severity is read off the text the
/// way the house renderer already reads it.
fn notice_cell(message: &str, width: usize) -> Vec<Line<'static>> {
    let lower = message.trim_start().to_ascii_lowercase();
    let (glyph, style) = if lower.starts_with("error") {
        ("■ ", theme::style(Token::Error))
    } else if lower.starts_with("warn") || message.trim_start().starts_with('⚠') {
        ("⚠ ", warning())
    } else {
        ("• ", dim())
    };
    let content_width = width.saturating_sub(LIVE_PREFIX_COLS as usize).max(1);
    let rows = super::wrap_all(
        message
            .lines()
            .map(|line| Line::from(Span::styled(line.to_string(), style)))
            .collect(),
        content_width,
    );
    // The error glyph shares the message's style rather than being a separate
    // dim bullet, which is what makes an error notice read as one red line.
    super::prefix_rows(rows, glyph, "  ", style)
}

// ---------------------------------------------------------------------------
// Exec and tool cells
// ---------------------------------------------------------------------------

/// Which Codex cell a Wizard tool call is rendered as.
///
/// Codex's transcript has a handful of purpose-built cells rather than one
/// generic tool card, and Wizard's built-in tools land on them cleanly. What is
/// left — MCP servers, `web_search`, `memory`, anything a loadout adds — takes
/// the MCP cell, which is exactly the cell Codex uses for tools it does not
/// know the shape of (`history_cell/mcp.rs:121-211`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// `• Ran <cmd>` with the command syntax-highlighted. `exec_cell/`.
    Exec,
    /// `• Explored` with `Read`/`List`/`Search` rows. `exec_cell/render.rs:255-350`.
    Explore,
    /// `• Edited <path>`. `diff_render.rs:410-472`.
    Patch,
    /// A delegated run, in the shape Codex gives a background agent.
    Agent,
    /// A detached command, in Codex's background-terminal grammar.
    /// `history_cell/exec.rs:21-64`.
    Background,
    /// `• Called <tool>(<args>)`. `history_cell/mcp.rs`.
    Call,
}

impl Shape {
    fn of(tool: &ToolItem) -> Shape {
        match tool.name.as_str() {
            // `run_in_background` is what turns an `execute` into one of
            // Codex's background terminals rather than a foreground `Ran`.
            "execute" => match tool.args.get("run_in_background").and_then(|v| v.as_bool()) {
                Some(true) => Shape::Background,
                _ => Shape::Exec,
            },
            "task_output" | "task_kill" => Shape::Background,
            "read_file" | "list_files" | "search_files" => Shape::Explore,
            "write_file" | "edit_file" => Shape::Patch,
            "spawn_subagent" => Shape::Agent,
            _ => Shape::Call,
        }
    }

    /// The header's verb: present participle while the call is in flight, past
    /// tense once it lands. `exec_cell/render.rs:365-374`, `mcp.rs:135-139`.
    fn verb(self, name: &str, running: bool) -> &'static str {
        match (self, running) {
            (Shape::Exec, true) => "Running",
            (Shape::Exec, false) => "Ran",
            (Shape::Explore, true) => "Exploring",
            (Shape::Explore, false) => "Explored",
            // Codex derives `Added`/`Edited` from the patch itself
            // (`diff_render.rs:429-433`); Wizard's two write tools say the same
            // thing by name, so the verb comes from the tool.
            (Shape::Patch, running) => match (name, running) {
                ("write_file", true) => "Adding",
                ("write_file", false) => "Added",
                (_, true) => "Editing",
                (_, false) => "Edited",
            },
            (Shape::Agent, true) => "Spawning",
            (Shape::Agent, false) => "Spawned",
            // The background-terminal cell builds its own header (the verb is
            // inside a single bold span there, `exec.rs:30-34`), so this is
            // only reached as a fallback.
            (Shape::Background, _) => "Background terminal",
            (Shape::Call, true) => "Calling",
            (Shape::Call, false) => "Called",
        }
    }
}

/// The bullet leading an exec cell: green on exit 0, red on failure, animated
/// while the call is live (`exec_cell/render.rs:357-364`).
///
/// HOUSE RULE: upstream uses the same `•` for both terminal states and lets the
/// hue carry "this failed". Under `NO_COLOR`, at 16 colors, or under the
/// `minimal` palette that distinction disappears entirely, so failure keeps a
/// `✗`. It is the same trade the `grok` skin's chrome table records, and for
/// the same reason: of everything a hue could be carrying, "the tool failed" is
/// the one the user cannot afford to miss.
fn tool_bullet(running: bool, failed: bool, tick: u64) -> Span<'static> {
    match (running, failed) {
        (true, _) => activity_marker(tick),
        (false, false) => Span::styled("•", theme::style(Token::ToolDone).bold()),
        (false, true) => Span::styled("✗", theme::style(Token::ToolFailed).bold()),
    }
}

/// One tool call, as the Codex cell its shape calls for.
///
/// `folded` is Wizard's own idea — a card the user has collapsed with a click
/// or Ctrl-T. Codex has no equivalent (its cells are fixed-size by
/// construction), so a folded card keeps its header and says how much it is
/// hiding, in the same `… +N lines` grammar the elision uses.
fn tool_cell(tool: &ToolItem, folded: bool, tick: u64, width: usize) -> Vec<Line<'static>> {
    let result = tool.output.as_ref();
    // A call with no result is still running — which is not the same as having
    // no body, because a foreground command streams its output into the card
    // while it works.
    let running = result.is_none();
    let failed = result.is_some_and(|result| result.is_error);
    let output = match result {
        Some(result) => Some(result.content.as_str()),
        None if !tool.progress.is_empty() => Some(tool.progress.as_str()),
        None => None,
    };
    let shape = Shape::of(tool);
    let verb = shape.verb(&tool.name, running);

    // `[bullet, " ", title.bold(), " "]` — `exec_cell/render.rs:376-380`.
    let mut header = vec![
        tool_bullet(running, failed, tick),
        Span::raw(" "),
        Span::styled(verb, text_style().bold()),
        Span::raw(" "),
    ];
    let header_prefix = 2 + verb.width() + 1;
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut body: Vec<Line<'static>> = Vec::new();

    match shape {
        Shape::Exec => {
            let command = arg_str(tool, "command").unwrap_or_default();
            let (head, rest) = command_display_lines(&command, width, header_prefix);
            header.extend(head);
            lines.push(Line::from(header));
            lines.extend(rest);
        }
        Shape::Explore => {
            lines.push(Line::from(header));
            body.extend(explore_rows(tool, width));
        }
        Shape::Patch => {
            let path = arg_str(tool, "path").unwrap_or_else(|| tool.name.clone());
            header.push(Span::styled(path, text_style()));
            lines.push(super::truncate_line(Line::from(header), width));
        }
        Shape::Agent => {
            // `title_with_agent`, `multi_agents.rs:475-482`: the verb bold,
            // then the agent's own label in the accent.
            let who = arg_str(tool, "subagent").unwrap_or_else(|| "agent".to_string());
            let task = arg_str(tool, "task").unwrap_or_default();
            header.extend(agent_label_spans(&who, "", false));
            if !task.is_empty() {
                header.push(Span::styled(" · ", dim()));
                header.push(Span::styled(task, dim()));
            }
            lines.push(super::truncate_line(Line::from(header), width));
        }
        Shape::Background => {
            // `history_cell/exec.rs:21-64`. A cell that only waited keeps the
            // bullet *inside* the bold span; one that interacted leads with a
            // dim `↳ ` instead, which is what distinguishes "this started
            // something" from "this reached into something already running".
            let command = arg_str(tool, "command").unwrap_or_default();
            let interacted = tool.name != "execute";
            let title = match tool.name.as_str() {
                "task_kill" => "Interrupted background terminal",
                "task_output" => "Interacted with background terminal",
                _ => "Started background terminal",
            };
            let mut spans = if interacted {
                vec![
                    Span::styled("↳ ", dim()),
                    Span::styled(title, text_style().bold()),
                ]
            } else {
                vec![
                    tool_bullet(running, failed, tick),
                    Span::raw(" "),
                    Span::styled(title, text_style().bold()),
                ]
            };
            if !command.is_empty() {
                spans.push(Span::styled(" · ", dim()));
                spans.push(Span::styled(command, dim()));
            }
            lines.push(super::truncate_line(Line::from(spans), width));
        }
        Shape::Call => {
            // `tool(args)`, appended inline when it fits on the header line and
            // hung off the arm when it does not — `mcp.rs:149-162`.
            let (label, summary) =
                super::tool_label(&tool.name, &tool.args, skin::ToolLabel::Plain);
            let invocation = if summary.is_empty() {
                label
            } else {
                format!("{label}({summary})")
            };
            if header_prefix + invocation.width() <= width {
                header.push(Span::styled(invocation, text_style()));
                lines.push(Line::from(header));
            } else {
                lines.push(Line::from(header));
                body.extend(super::wrap_all(
                    vec![Line::from(Span::styled(invocation, text_style()))],
                    prefixed_block(width, OUTPUT_ARM),
                ));
            }
        }
    }

    // An exploring cell has no output block at all: the `Read`/`Search` rows
    // *are* the summary, and hanging a file's contents under them would make
    // `Explored` the tallest cell in the transcript
    // (`exec_cell/render.rs:255-350` emits its rows and stops).
    //
    // Whitespace-only output counts as none, so a command that printed nothing
    // but a newline falls through to `(no output)` rather than growing an arm
    // with a blank row under it.
    let output = output.filter(|text| !text.trim().is_empty());
    if !matches!(shape, Shape::Explore) {
        match (folded, output) {
            // A folded card keeps its arm and says what is under it, in the
            // same grammar the elision uses — which is what a fold *is*. Hung
            // off the arm rather than appended to the header: a header with a
            // count trailing it reads as part of the command, and the arm is
            // where "there is more here" already lives.
            (true, Some(text)) => body.push(Line::from(Span::styled(
                format!("… +{} lines ({EXPAND_HINT})", text.lines().count()),
                dim(),
            ))),
            (false, Some(text)) => body.extend(output_rows(text, width, running)),
            _ => {}
        }
    }
    if !body.is_empty() {
        lines.extend(arm(body, OUTPUT_ARM));
    } else if matches!(shape, Shape::Exec) && !running {
        // A finished command with nothing to show still gets its arm, so the
        // cell does not look truncated. `exec_cell/render.rs:452-459`.
        lines.push(Line::from(vec![
            Span::styled(OUTPUT_ARM.0, dim()),
            Span::styled("(no output)", dim()),
        ]));
    }
    lines
}

/// A string argument off a tool call, when it has one.
fn arg_str(tool: &ToolItem, key: &str) -> Option<String> {
    tool.args
        .get(key)
        .and_then(|value| value.as_str())
        .map(str::to_string)
}

/// The command as it is shown next to `Ran`, plus its continuation rows.
///
/// `exec_cell/render.rs:352-497`. The shape is deliberate and easy to get
/// wrong: the *first* line of the highlighted script wraps against the width
/// left after the header prefix (`"• Ran "`), and only its first segment goes
/// on the header. Everything after that wraps at `width - 4` and hangs under a
/// dim `  │ ` gutter, capped at two rows by `limit_lines_from_start`
/// (`render.rs:499-510`) with a bare `… +N lines` — that elision carries **no**
/// transcript hint, unlike the output block's (`ellipsis_line`, `render.rs:621-623`).
fn command_display_lines(
    command: &str,
    width: usize,
    header_prefix: usize,
) -> (Vec<Span<'static>>, Vec<Line<'static>>) {
    // The script is highlighted as bash, exactly as `highlight_bash_to_lines`
    // does (`render/highlight.rs:690-692`); the shared highlighter already
    // memoizes and already degrades to plain lines.
    let script = super::highlight_code_block("bash", command);
    let head_width = width.saturating_sub(header_prefix).max(1);
    let mut rest: Vec<Line<'static>> = Vec::new();
    let mut head: Vec<Span<'static>> = Vec::new();

    let cont_width = prefixed_block(width, COMMAND_ARM);
    for (index, line) in script.into_iter().enumerate() {
        if index == 0 {
            let mut wrapped = super::wrap_lines(Text::from(vec![line]), head_width);
            if !wrapped.is_empty() {
                head = wrapped.remove(0).spans;
            }
            // Segments the header could not hold rewrap to the continuation
            // width rather than keeping the header's ragged edge.
            for line in wrapped {
                rest.extend(super::wrap_lines(Text::from(vec![line]), cont_width));
            }
        } else {
            rest.extend(super::wrap_lines(Text::from(vec![line]), cont_width));
        }
    }

    if rest.len() > COMMAND_CONTINUATION_MAX_LINES {
        let omitted = rest.len() - COMMAND_CONTINUATION_MAX_LINES;
        rest.truncate(COMMAND_CONTINUATION_MAX_LINES);
        rest.push(Line::from(Span::styled(
            format!("… +{omitted} lines"),
            dim(),
        )));
    }
    (head, arm(rest, COMMAND_ARM))
}

/// The rows of an "exploring" cell: one `Read`/`List`/`Search` verb per call,
/// the verb in the accent and the subject after it.
/// `exec_cell/render.rs:255-350`, where `Search` with both a query and a path
/// renders `query` + `" in "` dim + `path`.
fn explore_rows(tool: &ToolItem, width: usize) -> Vec<Line<'static>> {
    let content_width = prefixed_block(width, OUTPUT_ARM);
    let (verb, mut spans) = match tool.name.as_str() {
        "list_files" => (
            "List ",
            vec![Span::styled(
                arg_str(tool, "path").unwrap_or_else(|| ".".to_string()),
                text_style(),
            )],
        ),
        "search_files" => {
            let mut spans = vec![Span::styled(
                arg_str(tool, "pattern").unwrap_or_default(),
                text_style(),
            )];
            if let Some(path) = arg_str(tool, "path") {
                spans.push(Span::styled(" in ", dim()));
                spans.push(Span::styled(path, text_style()));
            }
            ("Search ", spans)
        }
        _ => (
            "Read ",
            vec![Span::styled(
                arg_str(tool, "path").unwrap_or_default(),
                text_style(),
            )],
        ),
    };
    let mut row = vec![Span::styled(verb, accent())];
    row.append(&mut spans);
    super::wrap_all(vec![Line::from(row)], content_width)
}

/// A tool's output, wrapped, and then elided from the *middle* when it will not
/// fit the row budget.
///
/// The order matters and is called out at `exec_cell/render.rs:461-463`: wrap
/// first, prefix second, truncate by rendered rows third. Truncating raw lines
/// before wrapping would let a handful of very long lines flood the viewport.
fn output_rows(text: &str, width: usize, running: bool) -> Vec<Line<'static>> {
    let content_width = prefixed_block(width, OUTPUT_ARM);
    let rows = super::wrap_all(
        text.lines()
            .map(|line| Line::from(Span::styled(line.to_string(), muted())))
            .collect(),
        content_width,
    );
    if rows.is_empty() {
        return vec![Line::from(Span::styled("(no output)", dim()))];
    }
    if running {
        // A running command is read from the bottom: the line it is waiting on
        // is the last one. Upstream's live cells behave the same way
        // (`exec_cell/live_output.rs`), retaining a tail rather than a head.
        let over = rows.len().saturating_sub(TOOL_CALL_MAX_LINES);
        if over == 0 {
            return rows;
        }
        let mut out = vec![Line::from(Span::styled(
            format!("… +{over} lines ({EXPAND_HINT})"),
            dim(),
        ))];
        out.extend(rows.into_iter().skip(over));
        return out;
    }
    truncate_rows_middle(rows, TOOL_CALL_MAX_LINES)
}

/// Keep the head and the tail of `rows`, with one elision line between them.
///
/// Ported from `ExecCell::truncate_lines_middle`, `exec_cell/render.rs:528-619`.
/// Upstream measures each line's cost in *viewport rows* via
/// `Paragraph::line_count`, because it truncates lines that may still wrap.
/// Here every row has already been wrapped to `width - 4` and prefixed with a
/// four-column gutter, so each costs exactly one row and the measurement
/// collapses to a count — the split, the budget halves and the reported count
/// (logical lines, so it stays stable across a resize) are upstream's.
fn truncate_rows_middle(rows: Vec<Line<'static>>, max_rows: usize) -> Vec<Line<'static>> {
    if max_rows == 0 {
        return Vec::new();
    }
    if rows.len() <= max_rows {
        return rows;
    }
    // One row is spent on the elision itself, so the budget the content gets is
    // what is left after it.
    let available = max_rows - 1;
    let head_budget = available / 2;
    let tail_budget = available - head_budget;
    let omitted = rows.len() - head_budget - tail_budget;
    let mut out: Vec<Line<'static>> = rows.iter().take(head_budget).cloned().collect();
    out.push(Line::from(Span::styled(
        format!("… +{omitted} lines ({EXPAND_HINT})"),
        dim(),
    )));
    out.extend(rows.into_iter().skip(head_budget + omitted));
    out
}

// ---------------------------------------------------------------------------
// The status indicator
// ---------------------------------------------------------------------------

/// `0s`, `59s`, `1m 00s`, `59m 59s`, `1h 00m 00s`, `25h 02m 03s`.
///
/// Ported from `status_indicator_widget.rs:65-78`. Note this is *not* the same
/// formatter the exec transcript uses (`format_duration`, below): that one has
/// no hour unit and spells sub-minute durations `1.50s`.
fn fmt_elapsed_compact(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!(
            "{}h {:02}m {:02}s",
            secs / 3600,
            (secs % 3600) / 60,
            secs % 60
        )
    }
}

/// `250ms`, `1.50s`, `1m 15s`.
///
/// Ported from `codex-rs/utils/elapsed/src/lib.rs:9-24`. Used for the rail's
/// per-run clock: Wizard's `ToolItem` carries no duration, so the exec cell has
/// nothing to print here — which costs nothing, since upstream does not show a
/// duration in the main viewport either (it appears only under Ctrl+T,
/// `exec_cell/render.rs:195-239`).
fn format_duration(secs: u64, millis: u128) -> String {
    if millis < 1000 {
        format!("{millis}ms")
    } else if secs < 60 {
        format!("{:.2}s", millis as f64 / 1000.0)
    } else {
        format!("{}m {:02}s", secs / 60, secs % 60)
    }
}

/// The bullet that marks something live.
///
/// `exec_cell/render.rs:177-184` → `motion.rs:35-76`: a shimmering `•` where
/// there is truecolor to shimmer in, and otherwise a `•`/`◦` blink on a 600 ms
/// half-period. The tick here runs at roughly 10 Hz, so six ticks is that
/// half-period.
fn activity_marker(tick: u64) -> Span<'static> {
    if theme::active().depth() == ColorDepth::TrueColor && terminal_bg().is_some() {
        motion::shimmer("•", tick)
            .into_iter()
            .next()
            .unwrap_or_else(|| Span::styled("•", dim()))
    } else if (tick / 6).is_multiple_of(2) {
        Span::styled("•", theme::style(Token::ToolRunning))
    } else {
        Span::styled("◦", dim())
    }
}

/// The turn's status on its own row above the composer, plus its details.
///
/// `status_indicator_widget.rs:253-282`. The punctuation is exact:
/// `(` + elapsed + ` • ` (space, U+2022, space) + key label + ` to interrupt)`,
/// all dim, with only the header shimmering.
///
/// Two Wizard facts ride along, because a skin restyles the UI and does not
/// withhold from it: the step counter (nothing else here has a step budget, so
/// there is no upstream slot for it — it goes inside the parentheses, ahead of
/// the elapsed time) and the queued-message count, which takes the inline
/// message slot Codex uses for `· 2 background terminals`.
fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let elapsed = app
        .turn_started
        .map(|started| started.elapsed().as_secs())
        .unwrap_or(0);

    let mut spans = vec![activity_marker(app.tick), Span::raw(" ")];
    // "Working" is Codex's word for it and the one the status row is
    // recognized by; Wizard's own themed verb ("Conjuring", "Scrying") goes on
    // a details line below, where it is flavour rather than chrome.
    spans.extend(motion::shimmer("Working", app.tick));
    spans.push(Span::raw(" "));

    let step = match app.status.max_steps.cap() {
        Some(cap) => format!("step {}/{cap}", app.status.step),
        None => format!("step {}", app.status.step),
    };
    // Wizard interrupts on Ctrl-C, not Esc; the phrasing is Codex's and the key
    // is the one that works.
    spans.push(Span::styled(
        format!(
            "({step} • {} • ctrl + c to interrupt)",
            fmt_elapsed_compact(elapsed)
        ),
        dim(),
    ));
    if !app.message_queue.is_empty() {
        spans.push(Span::styled(" · ", dim()));
        spans.push(Span::styled(
            format!("{} queued", app.message_queue.len()),
            dim(),
        ));
    }

    let mut lines = vec![super::truncate_line(Line::from(spans), area.width as usize)];
    for detail in status_details(app, area.width) {
        lines.push(detail);
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), area);
}

/// The status widget's details block: a `  └ ` arm, blanks under it, all dim,
/// capped at three rows. `status_indicator_widget.rs:200-230`.
///
/// What goes in it is whatever Wizard is doing that the one-line header cannot
/// say: the themed spinner verb, a model rebuild, MCP still connecting, and
/// compaction — which brings its own animated bar, because compaction is one
/// opaque call with no progress to report.
fn status_details(app: &App, width: u16) -> Vec<Line<'static>> {
    let content_width = prefixed_block(width as usize, DETAILS_ARM);
    let mut rows: Vec<Line<'static>> = Vec::new();
    if app.compacting {
        let mut spans = vec![Span::styled("compacting… ", dim())];
        spans.extend(
            super::indeterminate_bar(content_width.saturating_sub(13).max(4), app.tick).spans,
        );
        rows.push(Line::from(spans));
    }
    if let Some(label) = &app.rebuilding {
        rows.push(Line::from(Span::styled(format!("{label}…"), dim())));
    } else if app.status.busy {
        rows.push(Line::from(Span::styled(
            format!("{}…", app.spinner_verb),
            dim().italic(),
        )));
    }
    if app.mcp_connecting {
        rows.push(Line::from(Span::styled("connecting tools…", dim())));
    }
    let rows = super::wrap_all(rows, content_width);
    let mut out = arm(rows, DETAILS_ARM);
    // Overflow past the budget truncates rather than growing the widget, which
    // would shove the transcript up a row every time a detail arrived.
    if out.len() > STATUS_DETAILS_MAX_LINES {
        out.truncate(STATUS_DETAILS_MAX_LINES);
        if let Some(last) = out.last_mut() {
            *last = super::truncate_line(last.clone(), width.saturating_sub(1).max(1) as usize);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// The composer
// ---------------------------------------------------------------------------

/// The composer: no border, no rules, a `›` in the margin and a full-rect
/// background.
///
/// `bottom_pane/chat_composer.rs:936-990` for the geometry —
/// `Insets::tlbr(1, LIVE_PREFIX_COLS, 1, 1)`, so one blank row above the draft,
/// one below, two columns of prompt gutter and one column held back at the
/// right — and `:4751-4752` for the background, which is painted over the
/// *whole* rect including both blank rows. That block is what turns three rows
/// into a panel; without it the prompt glyph floats in space.
fn draw_composer(frame: &mut Frame, app: &App, area: Rect) {
    if area.width < 4 || area.height == 0 {
        return;
    }
    let bg = slab_bg();
    if let Some(color) = bg {
        // Painted first and patched over: ratatui's `Cell::set_style` only
        // overrides the fields a style actually sets, so text rendered after
        // this keeps the background underneath it.
        frame.render_widget(Block::default().style(Style::default().bg(color)), area);
    }

    // Too short for the full three rows. Drawing nothing here used to leave the
    // bottom of a four-row terminal completely blank, which reads as a hang;
    // the last row gets the prompt and the draft instead.
    let inner = if area.height < 3 {
        Rect {
            y: area.bottom().saturating_sub(1),
            height: 1,
            ..area
        }
    } else {
        Rect {
            y: area.y + 1,
            height: area.height - 2,
            ..area
        }
    };

    // While a command owns the composer, the top inset says so. Codex puts its
    // bash-mode signal in the prompt glyph alone; Wizard's console is a
    // *different destination* for Enter rather than a different parser, and a
    // banner is the loud half of saying so.
    if let Some(console) = &app.console
        && area.height >= 3
    {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" ▶ stdin → ", on_slab(warning().bold(), bg)),
                Span::styled(
                    super::truncate_width(&console.command, 48),
                    on_slab(warning(), bg),
                ),
            ])),
            Rect { height: 1, ..area },
        );
    }

    let budget = composer_budget(area.width);
    let chars = super::composer_chars(app);
    let cursor = app.cursor.min(chars.len());
    let normal = app.vim.is_normal();
    let rows = super::wrap_rows(&chars, budget);
    let (crow, ccol) = super::cursor_visual(&rows, cursor);

    // Vertical window: keep the cursor row in view.
    let height = (inner.height as usize).max(1);
    let top = crow.saturating_sub(height.saturating_sub(1));
    let last = (top + height).min(rows.len());

    let block_cursor = Style::default().add_modifier(Modifier::REVERSED);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut cursor_xy: Option<(u16, u16)> = None;

    for index in top..last {
        let (start, end) = rows[index];
        let row: &[char] = &chars[start..end];
        let widths: Vec<usize> = row
            .iter()
            .map(|ch| unicode_width::UnicodeWidthChar::width(*ch).unwrap_or(0))
            .collect();
        let is_cursor_row = index == crow;

        // The glyph and its states, `chat_composer.rs:4758-4781`: `›` bold
        // normally, `!` light red and bold in bash mode, `›` dim when input is
        // disabled. It is written at `textarea_rect.x - LIVE_PREFIX_COLS`, i.e.
        // column 0 of the composer, with column 1 left to the background.
        let prompt = if index > top {
            Span::styled("  ", on_slab(text_style(), bg))
        } else if app.console.is_some() {
            Span::styled("! ", on_slab(warning().bold(), bg))
        } else {
            Span::styled("› ", on_slab(text_style().bold(), bg))
        };
        let mut spans = vec![prompt];

        if normal && is_cursor_row {
            // Vim Normal mode paints its own block cursor, so the mode is
            // legible without a hardware caret.
            let rel = ccol.min(row.len());
            spans.push(Span::styled(
                row[..rel].iter().collect::<String>(),
                on_slab(text_style(), bg),
            ));
            if rel < row.len() {
                spans.push(Span::styled(
                    row[rel].to_string(),
                    on_slab(block_cursor, bg),
                ));
                spans.push(Span::styled(
                    row[rel + 1..].iter().collect::<String>(),
                    on_slab(text_style(), bg),
                ));
            } else {
                spans.push(Span::styled(" ", on_slab(block_cursor, bg)));
            }
        } else {
            spans.push(Span::styled(
                row.iter().collect::<String>(),
                on_slab(text_style(), bg),
            ));
            if let Some(ghost) = ghost_text(app, normal, &rows, cursor, chars.len(), is_cursor_row)
            {
                let used: usize = widths.iter().sum();
                let room = budget.saturating_sub(used);
                if room > 0 {
                    let ghost: String = ghost.chars().take(room).collect();
                    spans.push(Span::styled(ghost, on_slab(dim().italic(), bg)));
                }
            }
            if is_cursor_row && !normal {
                let cols: usize = widths[..ccol.min(widths.len())].iter().sum();
                cursor_xy = Some((
                    inner.x + LIVE_PREFIX_COLS + cols as u16,
                    inner.y + (index - top) as u16,
                ));
            }
        }
        lines.push(slab_line(spans, area.width as usize, bg));
    }

    // The placeholder, dim, at the textarea's own column. `chat_composer.rs:4817-4830`.
    if chars.is_empty()
        && app.console.is_none()
        && let Some(first) = lines.first_mut()
    {
        first.spans.truncate(1);
        first.spans.push(Span::styled(
            super::truncate_width(PLACEHOLDER, budget),
            on_slab(dim(), bg),
        ));
        *first = slab_line(std::mem::take(&mut first.spans), area.width as usize, bg);
    }

    frame.render_widget(Paragraph::new(Text::from(lines)), inner);

    if !normal
        && app.picker.is_none()
        && app.plan_review.is_none()
        && app.interview.is_none()
        && let Some((x, y)) = cursor_xy
    {
        frame.set_cursor_position(Position::new(x, y));
    }
}

/// The inline completion shown after the draft, when there is one to show.
///
/// Only on a single-row draft with the cursor at the very end, where `→`
/// accepts it — anywhere else it would be a suggestion the user cannot act on.
fn ghost_text(
    app: &App,
    normal: bool,
    rows: &[(usize, usize)],
    cursor: usize,
    len: usize,
    is_cursor_row: bool,
) -> Option<String> {
    if normal || !is_cursor_row || rows.len() != 1 || cursor != len {
        return None;
    }
    if app.picker.is_some() || app.input_mode != InputMode::Command {
        return None;
    }
    let spec = app.suggestions.get(app.suggestion_index)?;
    let typed = app.input.trim_start().strip_prefix('/')?;
    let remainder = spec.name.strip_prefix(typed)?;
    let mut ghost = remainder.to_string();
    if !spec.args.is_empty() {
        ghost.push(' ');
        ghost.push_str(&spec.args);
    }
    (!ghost.is_empty()).then_some(ghost)
}

// ---------------------------------------------------------------------------
// The footer
// ---------------------------------------------------------------------------

/// Which hint the footer's left side is currently offering.
/// `bottom_pane/footer.rs:295-343`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HintKind {
    None,
    /// `? for shortcuts` — and it is true: typing `?` into an empty composer
    /// puts [`FooterMode::ShortcutOverlay`] on screen, which is exactly the
    /// gesture Codex binds it to.
    Shortcuts,
    QueueMessage,
    QueueShort,
}

/// Which footer content is on screen. `bottom_pane/footer.rs:162-179`.
///
/// Four of Codex's six variants have a Wizard trigger and are listed here.
/// `HistorySearch` needs a reverse-i-search Wizard does not have, and `EscHint`
/// needs Esc-Esc to edit the previous message, which Esc does not do here — a
/// footer that offered either would be naming a key that does nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FooterMode {
    /// The multi-line two-column shortcut list, shown while the draft is `?`.
    ShortcutOverlay,
    /// "press again to exit", while Ctrl-C is armed.
    QuitShortcutReminder,
    /// The base row with an empty composer.
    ComposerEmpty,
    /// The base row with a draft in the composer.
    ComposerHasDraft,
}

impl FooterMode {
    fn of(app: &App) -> FooterMode {
        if app.input.trim() == "?" && app.suggestions.is_empty() {
            FooterMode::ShortcutOverlay
        } else if app.ctrl_c_armed {
            FooterMode::QuitShortcutReminder
        } else if app.input.is_empty() {
            FooterMode::ComposerEmpty
        } else {
            FooterMode::ComposerHasDraft
        }
    }
}

/// The shortcut list, in two columns.
///
/// `shortcut_overlay_lines` + `build_columns`, `bottom_pane/footer.rs:887-997`:
/// entries are laid out row-major into two columns, column 0 padded to its
/// widest entry plus `COLUMN_PADDING` (4) plus `COLUMN_GAP` (4), every row dim,
/// then a blank line and `customize shortcuts with ` + a cyan command.
///
/// The keys are Wizard's. Codex's `!` for shell commands, `@` for file paths
/// and `⌥ + ,` for reasoning effort have no counterpart here and are not
/// listed; listing them would be advertising bindings that do nothing.
fn shortcut_overlay_lines(app: &App) -> Vec<Line<'static>> {
    let entries: Vec<String> = vec![
        "/ for commands".to_string(),
        "enter to submit message".to_string(),
        "shift + enter for newline".to_string(),
        if app.status.busy {
            "enter to queue message".to_string()
        } else {
            "↑ for history".to_string()
        },
        "ctrl + t to expand the last tool".to_string(),
        "shift + tab to change mode".to_string(),
        "ctrl + r to rewind".to_string(),
        if app.status.busy {
            "ctrl + c to interrupt".to_string()
        } else {
            "ctrl + c to exit".to_string()
        },
    ];
    const COLUMNS: usize = 2;
    const COLUMN_PADDING: usize = 4;
    const COLUMN_GAP: usize = 4;
    let column_width = entries
        .iter()
        .step_by(COLUMNS)
        .map(|entry| entry.width())
        .max()
        .unwrap_or(0)
        + COLUMN_PADDING;
    let mut lines: Vec<Line<'static>> = entries
        .chunks(COLUMNS)
        .map(|chunk| {
            let mut spans = vec![Span::styled(chunk[0].clone(), dim())];
            if let Some(right) = chunk.get(1) {
                let pad = column_width.saturating_sub(chunk[0].width()) + COLUMN_GAP;
                spans.push(Span::raw(" ".repeat(pad)));
                spans.push(Span::styled(right.clone(), dim()));
            }
            Line::from(spans)
        })
        .collect();
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("every command and key with ", dim()),
        Span::styled("/help", accent()),
    ]));
    lines
}

/// One candidate for the footer's left side: a hint, and whether the mode label
/// carries its cycle suffix. `footer.rs:300-304`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LeftState {
    hint: HintKind,
    cycle: bool,
}

/// What the collapse chose for the left side.
enum SummaryLeft {
    Line(Line<'static>),
    None,
}

/// Build the left side for one candidate state. `footer.rs:306-341`: the key
/// label and the words after it are both dim, and the mode label is joined on
/// with a dim ` · `.
fn left_side_line(mode: Option<&str>, state: LeftState) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    match state.hint {
        HintKind::None => {}
        HintKind::Shortcuts => {
            spans.push(Span::styled("?", dim()));
            spans.push(Span::styled(" for shortcuts", dim()));
        }
        HintKind::QueueMessage => {
            spans.push(Span::styled("enter", dim()));
            spans.push(Span::styled(" to queue message", dim()));
        }
        HintKind::QueueShort => {
            spans.push(Span::styled("enter", dim()));
            spans.push(Span::styled(" to queue", dim()));
        }
    }
    if let Some(mode) = mode {
        if !matches!(state.hint, HintKind::None) {
            spans.push(Span::styled(" · ", dim()));
        }
        let label = if state.cycle {
            format!("{mode} ({MODE_CYCLE_HINT})")
        } else {
            mode.to_string()
        };
        // Upstream paints this magenta; the palette here is the theme's, and
        // `accent` is the token `assets/themes/codex.toml` reserves for the
        // things that are live or actionable — which a sticky mode is.
        spans.push(Span::styled(label, accent()));
    }
    Line::from(spans)
}

/// Where a right-aligned line of `content_width` starts, with the two-column
/// right padding upstream keeps. `footer.rs:601-622`.
fn right_aligned_x(area: Rect, content_width: u16) -> Option<u16> {
    if area.is_empty() {
        return None;
    }
    let max_width = area.width.saturating_sub(FOOTER_INDENT_COLS);
    if content_width == 0 || max_width == 0 {
        return None;
    }
    if content_width >= max_width {
        return Some(area.x.saturating_add(FOOTER_INDENT_COLS));
    }
    Some(
        area.x
            .saturating_add(area.width)
            .saturating_sub(content_width)
            .saturating_sub(FOOTER_INDENT_COLS),
    )
}

/// Does the left side fit at all, ignoring the right? `footer.rs:288-291`.
fn left_fits(area: Rect, left_width: u16) -> bool {
    left_width <= area.width.saturating_sub(FOOTER_INDENT_COLS)
}

/// Does the left side fit *alongside* the right, with the one-column gap
/// between them? `footer.rs:638-647`.
fn can_show_left_with_context(area: Rect, left_width: u16, context_width: u16) -> bool {
    let Some(context_x) = right_aligned_x(area, context_width) else {
        return true;
    };
    if left_width == 0 {
        return true;
    }
    let left_extent = FOOTER_INDENT_COLS + left_width + FOOTER_CONTEXT_GAP_COLS;
    left_extent <= context_x.saturating_sub(area.x)
}

/// Choose what fits on the footer's single row, and whether the right-hand
/// context survives alongside it.
///
/// Ported from `single_line_footer_layout`, `bottom_pane/footer.rs:353-522`.
/// The order is the load-bearing part and is documented at `footer.rs:21-43`:
///
/// 1. the fullest left line with the context, if it fits;
/// 2. in queue mode, prefer keeping the queue hint over keeping the context —
///    try every queue variant with the context first, then every variant
///    without it;
/// 3. otherwise, drop the hint *before* dropping `(shift+tab to cycle)`; and if
///    the cycle hint was applicable but could not fit, suppress the context too
///    (`context_requires_cycle_hint`, `footer.rs:390`) so the right side never
///    outlives the cycle hint;
/// 4. mode label alone, with then without the context;
/// 5. nothing — and the context is still allowed (`footer.rs:521`).
fn single_line_footer_layout(
    area: Rect,
    context_width: u16,
    mode: Option<&str>,
    show_cycle_hint: bool,
    hint: HintKind,
) -> (SummaryLeft, bool) {
    let queue = matches!(hint, HintKind::QueueMessage | HintKind::QueueShort);
    let default_state = LeftState {
        hint,
        cycle: show_cycle_hint,
    };
    let line_of = |state: LeftState| left_side_line(mode, state);
    let width_of = |state: LeftState| line_of(state).width() as u16;

    let default_width = width_of(default_state);
    if default_width > 0 && can_show_left_with_context(area, default_width, context_width) {
        return (SummaryLeft::Line(line_of(default_state)), true);
    }

    // Only show the context when the cycle-hint variant can also fit, so the
    // right side does not flicker in and out one resize step before the left.
    let context_requires_cycle_hint = show_cycle_hint && !queue;

    if queue {
        let states = [
            default_state,
            LeftState {
                hint: HintKind::QueueMessage,
                cycle: false,
            },
            LeftState {
                hint: HintKind::QueueShort,
                cycle: false,
            },
        ];
        // Pass 1: keep the context if any queue variant fits beside it.
        for (pass, with_context) in [(0, true), (1, false)] {
            let _ = pass;
            let mut previous: Option<LeftState> = None;
            for state in states {
                if previous == Some(state) {
                    continue;
                }
                previous = Some(state);
                let width = width_of(state);
                let fits = if with_context {
                    can_show_left_with_context(area, width, context_width)
                } else {
                    left_fits(area, width)
                };
                if width > 0 && fits {
                    return (SummaryLeft::Line(line_of(state)), with_context);
                }
            }
        }
    } else if mode.is_some() {
        if show_cycle_hint {
            let cycle = LeftState {
                hint: HintKind::None,
                cycle: true,
            };
            let width = width_of(cycle);
            if width > 0 && can_show_left_with_context(area, width, context_width) {
                return (SummaryLeft::Line(line_of(cycle)), true);
            }
            if width > 0 && left_fits(area, width) {
                return (SummaryLeft::Line(line_of(cycle)), false);
            }
        }
        let mode_only = LeftState {
            hint: HintKind::None,
            cycle: false,
        };
        let width = width_of(mode_only);
        if !context_requires_cycle_hint
            && width > 0
            && can_show_left_with_context(area, width, context_width)
        {
            return (SummaryLeft::Line(line_of(mode_only)), true);
        }
        if width > 0 && left_fits(area, width) {
            return (SummaryLeft::Line(line_of(mode_only)), false);
        }
    }

    if mode.is_some() {
        let mode_only = LeftState {
            hint: HintKind::None,
            cycle: false,
        };
        let width = width_of(mode_only);
        if !context_requires_cycle_hint && can_show_left_with_context(area, width, context_width) {
            return (SummaryLeft::Line(line_of(mode_only)), true);
        }
        if left_fits(area, width) {
            return (SummaryLeft::Line(line_of(mode_only)), false);
        }
    }
    (SummaryLeft::None, true)
}

/// The one row under the composer.
///
/// Every footer row is drawn through a hard two-space left indent
/// (`footer.rs:249-256`) and the right-hand context is right-aligned with a
/// two-column right pad (`footer.rs:601-622`).
///
/// The transient, instructional states short-circuit the collapse entirely, the
/// way `footer_from_props_lines` does for the quit reminder and the Esc hint
/// (`footer.rs:700-764`): when a modal is up or a command owns the composer,
/// what the keys do *right now* matters more than ambient context, so those
/// rows are rendered whole.
fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let indent = " ".repeat(FOOTER_INDENT_COLS as usize);

    let mode = FooterMode::of(app);
    if mode == FooterMode::ShortcutOverlay {
        // The overlay bypasses the collapse entirely, exactly as the transient
        // instructional modes do upstream (`footer.rs:700-764`).
        let lines: Vec<Line<'static>> = shortcut_overlay_lines(app)
            .into_iter()
            .map(|line| {
                super::truncate_line(
                    Line::from(
                        std::iter::once(Span::raw(indent.clone()))
                            .chain(line.spans)
                            .collect::<Vec<_>>(),
                    ),
                    area.width as usize,
                )
            })
            .collect();
        frame.render_widget(Paragraph::new(Text::from(lines)), area);
        return;
    }

    if let Some(instruction) = instructional_footer(app, mode) {
        let line = Line::from(
            std::iter::once(Span::raw(indent))
                .chain(instruction.spans)
                .collect::<Vec<_>>(),
        );
        frame.render_widget(
            Paragraph::new(super::truncate_line(line, area.width as usize)),
            area,
        );
        return;
    }

    let mode_name = mode_label(app);
    let mode = mode_name;
    let hint = if app.status.busy && !app.input.is_empty() {
        HintKind::QueueMessage
    } else if app.input.is_empty() {
        HintKind::Shortcuts
    } else {
        HintKind::None
    };
    // Shift+Tab really does cycle the collaboration mode in Wizard (it toggles
    // plan mode), so the hint names something that works.
    let show_cycle_hint = mode.is_some() && !app.status.busy;

    // Two tiers of right-hand content: everything, and then only the sticky
    // state that must not vanish. Upstream has one (the context window) and
    // drops it wholesale; dropping Wizard's `ULTRA ×N` to make room for a token
    // count would hide the multiplier on what the next turn costs, so the meter
    // goes first instead.
    let full = context_line(app, true);
    let short = context_line(app, false);
    let mut right = full.clone();
    let (left, show_context) = {
        let (left, show) = single_line_footer_layout(
            area,
            full.width() as u16,
            mode.as_deref(),
            show_cycle_hint,
            hint,
        );
        if show || short.width() == 0 {
            (left, show)
        } else {
            right = short.clone();
            single_line_footer_layout(
                area,
                short.width() as u16,
                mode.as_deref(),
                show_cycle_hint,
                hint,
            )
        }
    };

    if let SummaryLeft::Line(line) = left {
        let line = Line::from(
            std::iter::once(Span::raw(indent))
                .chain(line.spans)
                .collect::<Vec<_>>(),
        );
        frame.render_widget(Paragraph::new(line), area);
    }
    if show_context && right.width() > 0 {
        let width = right.width() as u16;
        if let Some(x) = right_aligned_x(area, width) {
            let at = Rect {
                x,
                y: area.y,
                width: width.min(area.right().saturating_sub(x)),
                height: 1,
            };
            frame.render_widget(Paragraph::new(right), at);
        }
    }
}

/// The footer row for a state where what the keys do right now is the whole
/// message. `None` when the ordinary collapse applies.
fn instructional_footer(app: &App, mode: FooterMode) -> Option<Line<'static>> {
    let hint = |text: &'static str| Some(Line::from(Span::styled(text, dim())));
    if mode == FooterMode::QuitShortcutReminder {
        // `FooterMode::QuitShortcutReminder` (`footer.rs:829-841`), with
        // Wizard's verb: Ctrl-C exits, it does not quit to a menu.
        return Some(Line::from(vec![
            Span::styled("ctrl + c", dim()),
            Span::styled(" again to exit", dim()),
        ]));
    }
    if app.plan_review.is_some() {
        return match app.plan_review.as_ref().and_then(|r| r.feedback.as_ref()) {
            Some(_) => hint("type feedback · enter to reject · esc to go back"),
            None => hint("y to approve · n to reject · ↑↓ to scroll"),
        };
    }
    if app.interview.is_some() {
        return hint("1-9 to pick · type an answer · enter for next · esc to skip");
    }
    if app.picker.is_some() {
        return hint("↑↓ to move · enter to select · esc to cancel");
    }
    if !app.suggestions.is_empty() {
        return hint("↑↓ to select · tab to complete · enter to run");
    }
    if app.console.is_some() {
        // Codex's nearest thing is bash mode, whose footer says `Shell mode`.
        // Wizard's console is louder than that: Enter goes somewhere else
        // entirely while it is open.
        return Some(Line::from(vec![
            Span::styled("enter", dim()),
            Span::styled(" to send · ", dim()),
            Span::styled("ctrl + d", dim()),
            Span::styled(" to end input · ", dim()),
            Span::styled("esc", dim()),
            Span::styled(" to detach", dim()),
        ]));
    }
    if app.diff.is_some() {
        return hint("pgup/pgdn to scroll the diff · esc to close");
    }
    None
}

/// The collaboration-mode label, when one is on. `footer.rs:138-156`.
fn mode_label(app: &App) -> Option<String> {
    if app.omakase {
        Some("Omakase mode".to_string())
    } else if app.plan_mode {
        Some("Plan mode".to_string())
    } else {
        None
    }
}

/// The right-hand side: Wizard's ambient state, in the slot Codex reserves for
/// its context window.
///
/// `full` adds the two things that are context rather than state — the model
/// and the context meter — and is the tier the collapse drops first. What stays
/// is what is sticky and expensive: fusion, the ultra multiplier, sovereign
/// mode, detached work, and a provider that did not answer.
///
/// The meter itself follows `context_window_line` (`footer.rs:999-1011`): Codex
/// prints `{}% context left` when it knows the window and `{} used` when it
/// only knows the count. Wizard only ever knows the count, so it is always the
/// second form.
fn context_line(app: &App, full: bool) -> Line<'static> {
    let mut chips: Vec<Vec<Span<'static>>> = Vec::new();
    if full {
        chips.push(vec![super::model_span(app)]);
        if app.status.mode == Mode::Genie {
            chips.push(vec![super::mode_span(app.status.mode)]);
        }
    }
    if app.status.mode == Mode::Sovereign {
        chips.push(vec![super::mode_span(app.status.mode)]);
    }
    if app.fusion_active {
        chips.push(vec![Span::styled("fusion", accent().bold())]);
    }
    if let Some(ultra) = &app.ultra {
        chips.push(vec![Span::styled(
            format!("ULTRA \u{00d7}{}", ultra.candidates()),
            accent().bold(),
        )]);
    }
    for (count, noun) in [
        (app.status.background_tasks, "bg task"),
        (app.status.background_subagents, "bg subagent"),
    ] {
        if count > 0 {
            let plural = if count == 1 { "" } else { "s" };
            chips.push(vec![Span::styled(
                format!("⏵ {count} {noun}{plural}"),
                accent(),
            )]);
        }
    }
    if app.provider_health_error.is_some() {
        chips.push(vec![Span::styled("⚠ provider", warning().bold())]);
    }
    if full && app.status.context_tokens > 0 {
        chips.push(vec![Span::styled(
            format!(
                "{} used",
                crate::usage::format_tokens(app.status.context_tokens)
            ),
            dim(),
        )]);
    }

    let mut spans: Vec<Span<'static>> = Vec::new();
    for chip in chips {
        if !spans.is_empty() {
            spans.push(Span::styled(" · ", dim()));
        }
        spans.extend(chip);
    }
    Line::from(spans)
}

// ---------------------------------------------------------------------------
// The plan band (Wizard's todos) and the rail (Wizard's subagents)
// ---------------------------------------------------------------------------

/// Wizard's todo list, drawn as Codex's plan-update cell.
///
/// `history_cell/plans.rs:175-226`: a `• ` dim bullet with a bold title, then
/// the steps under a `  └ ` arm, each led by its own box. The step styles are
/// upstream's exactly (`plans.rs:188-190`): a completed step is struck through
/// *and* dim, the current one is bold in the accent, and a pending one is dim.
/// The three glyphs mean the three states carry without any color at all.
fn draw_plan_band(frame: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 || area.width < 6 {
        return;
    }
    use crate::tools::todo::TodoStatus;
    // `lines.push(vec!["• ".dim(), "Updated Plan".bold()].into())` —
    // `plans.rs:204`, and that is the whole header. No count, no dismiss hint:
    // the steps below already say how far along the plan is, and `esc` is in
    // the footer's key list where every other key of Wizard's lives.
    let mut lines = vec![Line::from(vec![
        Span::styled("• ", dim()),
        Span::styled("Updated Plan", text_style().bold()),
    ])];

    let visible = (area.height as usize).saturating_sub(1).max(1);
    let rows: Vec<Line<'static>> = if app.todos.is_empty() {
        vec![Line::from(Span::styled(
            "(no steps provided)",
            dim().italic(),
        ))]
    } else {
        // Scroll to keep the current step visible once the list outgrows the
        // band, the way the house band does.
        let focus = app
            .todos
            .iter()
            .position(|item| item.status == TodoStatus::InProgress)
            .unwrap_or(0);
        let start = if app.todos.len() <= visible {
            0
        } else {
            focus
                .saturating_sub(visible.saturating_sub(1) / 2)
                .min(app.todos.len() - visible)
        };
        app.todos
            .iter()
            .skip(start)
            .take(visible)
            .map(|item| {
                let (glyph, style) = match item.status {
                    TodoStatus::Completed => ("✔ ", dim().add_modifier(Modifier::CROSSED_OUT)),
                    TodoStatus::InProgress => ("□ ", accent().bold()),
                    TodoStatus::Pending => ("□ ", dim()),
                };
                super::truncate_line(
                    Line::from(vec![
                        Span::styled(glyph, style),
                        Span::styled(item.content.clone(), style),
                    ]),
                    prefixed_block(area.width as usize, OUTPUT_ARM),
                )
            })
            .collect()
    };
    lines.extend(arm(rows, OUTPUT_ARM));
    lines.truncate(area.height as usize);
    frame.render_widget(Paragraph::new(Text::from(lines)), area);
}

/// One agent's status, in the words and colors `multi_agents.rs:628-665` uses.
///
/// `Running` is cyan and bold, `Completed` green with a ` - ` dim separator
/// before a preview of what it said, `Error` red with the same separator. The
/// preview is squeezed to single spaces first, so a multi-line answer does not
/// arrive as a ragged fragment.
fn agent_status_spans(pane: &crate::app::SubagentPane) -> Vec<Span<'static>> {
    let preview = |text: &str, limit: usize| {
        super::truncate_width(
            &text.split_whitespace().collect::<Vec<_>>().join(" "),
            limit,
        )
    };
    match pane.status {
        PaneStatus::Running => vec![Span::styled(
            "Running",
            theme::style(Token::ToolRunning).bold(),
        )],
        PaneStatus::Done => {
            let mut spans = vec![Span::styled("Completed", theme::style(Token::ToolDone))];
            let summary = preview(pane.activity().trim(), 48);
            if !summary.is_empty() {
                spans.push(Span::styled(" - ", dim()));
                spans.push(Span::styled(summary, text_style()));
            }
            spans
        }
        // HOUSE RULE again: upstream's `Error` is a red word and nothing else.
        // The `✗` is what carries it at 16 colors.
        PaneStatus::Failed => {
            let mut spans = vec![Span::styled(
                "✗ Error",
                theme::style(Token::ToolFailed).bold(),
            )];
            let summary = preview(pane.activity().trim(), 48);
            if !summary.is_empty() {
                spans.push(Span::styled(" - ", dim()));
                spans.push(Span::styled(summary, text_style()));
            }
            spans
        }
    }
}

/// The agent label: nickname in the accent and bold, its role after it in
/// brackets. `multi_agents.rs:504-530` (`agent_label_spans`).
fn agent_label_spans(name: &str, role: &str, selected: bool) -> Vec<Span<'static>> {
    let style = if selected {
        accent().bold().add_modifier(Modifier::REVERSED)
    } else {
        accent().bold()
    };
    let mut spans = vec![Span::styled(name.to_string(), style)];
    if !role.is_empty() {
        spans.push(Span::styled(" ", dim()));
        spans.push(Span::styled(format!("[{role}]"), text_style()));
    }
    spans
}

/// Wizard's subagent rail, drawn as Codex's collaboration cell.
///
/// `multi_agents.rs:460-486` (`collab_event` / `title_spans_line`): a `• ` dim
/// bullet, a bold verb, the agent label, and then one detail line per agent
/// hanging off `  └ ` / `    ` in the form `label: Status - preview`.
///
/// Wizard's rail is *navigable* where Codex's cell is a readout — ↑/↓ move and
/// Enter opens a run — so the focused row is reversed rather than being given a
/// marker column, which is how Codex's own selection lists mark a choice
/// (`selection_popup_common.rs:334-350`, restyle rather than prefix).
fn draw_rail(frame: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 || area.width < 8 {
        return;
    }
    let focused = app.rail_focus;
    let selected = focused.or(app.attached);
    let visible = app.panes.len().min(MAX_RAIL_ROWS);
    let start = match selected {
        Some(index) if index >= visible => index + 1 - visible,
        _ => 0,
    };
    let end = (start + visible).min(app.panes.len());
    let live = app
        .panes
        .iter()
        .filter(|pane| pane.status == PaneStatus::Running)
        .count();

    // `waiting_begin`, `multi_agents.rs:385-393`, has three forms and picks by
    // *count*: one agent gets `title_with_agent("Waiting for", label)` — the
    // verb bold, then that agent's own label in cyan — and only two or more
    // fall back to the counted `Waiting for N agents`. Rendering the counted
    // form at N == 1 is both the wrong cell and the reason this used to read
    // "1 agents".
    let verb = if live > 0 { "Waiting for" } else { "Spawned" };
    let counted = if live > 0 { live } else { app.panes.len() };
    let mut header = vec![Span::styled("• ", dim())];
    match (
        counted,
        app.panes
            .iter()
            .find(|pane| live == 0 || pane.status == PaneStatus::Running),
    ) {
        (1, Some(pane)) => {
            header.push(Span::styled(format!("{verb} "), text_style().bold()));
            header.extend(agent_label_spans(&pane.name, "", false));
        }
        _ => header.push(Span::styled(
            format!("{verb} {counted} agents"),
            text_style().bold(),
        )),
    }
    let mut lines = vec![Line::from(header)];

    let content_width = prefixed_block(area.width as usize, OUTPUT_ARM);
    let mut rows: Vec<Line<'static>> = Vec::new();
    for (index, pane) in app.panes.iter().enumerate().take(end).skip(start) {
        let is_selected = selected == Some(index) && focused.is_some();
        let elapsed = pane.elapsed();
        let mut spans = agent_label_spans(&pane.name, "", is_selected);
        spans.push(Span::styled(": ", dim()));
        spans.extend(agent_status_spans(pane));
        // Wizard's own two facts about a run: how long it has been going, and
        // how much of it you have not read.
        spans.push(Span::styled(
            format!(
                "  {}",
                format_duration(elapsed.as_secs(), elapsed.as_millis())
            ),
            dim(),
        ));
        if pane.unread > 0 && Some(index) != app.attached {
            spans.push(Span::styled(format!(" +{}", pane.unread), accent().bold()));
        }
        rows.push(super::truncate_line(Line::from(spans), content_width));
    }
    if app.panes.len() > MAX_RAIL_ROWS {
        rows.push(Line::from(Span::styled(
            format!("+{} more", app.panes.len() - visible),
            dim().italic(),
        )));
    }
    lines.extend(arm(rows, OUTPUT_ARM));
    lines.truncate(area.height as usize);
    frame.render_widget(Paragraph::new(Text::from(lines)), area);
}

// ---------------------------------------------------------------------------
// Floating layers, on Codex's menu surface
// ---------------------------------------------------------------------------

/// Centre `size` inside `within`, clipped to it.
fn centred(within: Rect, width: u16, height: u16) -> Rect {
    Rect {
        x: within.x + within.width.saturating_sub(width) / 2,
        y: within.y + within.height.saturating_sub(height) / 2,
        width: width.min(within.width),
        height: height.min(within.height),
    }
}

/// The slash-command popup, stacked directly on top of the composer.
///
/// `bottom_pane/command_popup.rs` feeds `render_rows`
/// (`selection_popup_common.rs:455-530`): a menu surface, one row per command,
/// the name in a left column and its description dim in a right one, and the
/// selected row recognized by being entirely in the accent — no marker column,
/// no border.
fn draw_command_popup(frame: &mut Frame, app: &App, composer: Rect) {
    if app.suggestions.is_empty() {
        return;
    }
    const MAX_ROWS: usize = 8;
    let rows = app.suggestions.len().min(MAX_ROWS);
    let height = rows as u16 + MENU_SURFACE_INSET_V * 2;
    if composer.y < height || composer.width < 16 {
        return;
    }
    let area = Rect {
        x: composer.x,
        y: composer.y - height,
        width: composer.width,
        height,
    };
    let inner = menu_surface(frame, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    // Window the rows so the selection stays visible on a short terminal.
    let start = app.suggestion_index.saturating_sub(rows.saturating_sub(1));
    let label_col = app
        .suggestions
        .iter()
        .skip(start)
        .take(rows)
        .map(|spec| spec.name.width() + spec.args.width() + 2)
        .max()
        .unwrap_or(0)
        .min(inner.width as usize / 2)
        + 2;

    let lines: Vec<Line<'static>> = app
        .suggestions
        .iter()
        .enumerate()
        .skip(start)
        .take(rows)
        .map(|(index, spec)| {
            menu_row(
                format!("/{} {}", spec.name, spec.args)
                    .trim_end()
                    .to_string(),
                &spec.description,
                index == app.suggestion_index,
                label_col,
                inner.width as usize,
            )
        })
        .collect();
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

/// The model / mode / rewind / subagent picker.
///
/// `bottom_pane/list_selection_view.rs:352-390`: a bold title, a dim subtitle,
/// the rows, and a dim footer hint — all on a menu surface. The `●` marking
/// the current value is Wizard's, kept because "which one am I on" is the
/// question a picker exists to answer.
fn draw_picker(frame: &mut Frame, app: &App, body: Rect) {
    let Some(picker) = &app.picker else {
        return;
    };
    let width = body.width.saturating_sub(8).clamp(24, 72);
    // Title, blank, rows, blank, hint — plus the surface's own two rows.
    let max_rows = body.height.saturating_sub(7).max(1) as usize;
    let rows = picker.items.len().min(max_rows);
    let height = rows as u16 + 4 + MENU_SURFACE_INSET_V * 2;
    let inner = menu_surface(frame, centred(body, width, height));
    if inner.width < 4 || inner.height < 3 {
        return;
    }
    let content_width = inner.width as usize;

    let start = picker.selected.saturating_sub(rows.saturating_sub(1));
    let label_col = picker
        .items
        .iter()
        .skip(start)
        .take(rows)
        .map(|item| item.value.width() + usize::from(item.current) * 2)
        .max()
        .unwrap_or(0)
        .min(content_width / 2)
        + 2;

    let mut lines = vec![
        Line::from(Span::styled(picker.title.clone(), text_style().bold())),
        Line::from(""),
    ];
    for (index, item) in picker.items.iter().enumerate().skip(start).take(rows) {
        let label = if item.current {
            format!("{} ●", item.value)
        } else {
            item.value.clone()
        };
        lines.push(menu_row(
            label,
            &item.detail,
            index == picker.selected,
            label_col,
            content_width,
        ));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        super::truncate_width(picker.footer_hint(), content_width),
        dim(),
    )));
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

/// The plan-review modal: the plan, and a verdict prompt.
///
/// The nearest Codex surface is the approval overlay
/// (`bottom_pane/approval_overlay.rs`), which is the same menu surface with a
/// bold title and a dim hint under the body. The turn is parked inside
/// `exit_plan` until this is answered, which is why it floats over everything.
fn draw_plan_review(frame: &mut Frame, app: &App, body: Rect) {
    let Some(review) = &app.plan_review else {
        return;
    };
    let width = body.width.saturating_sub(6).clamp(24, 92);
    let height = body.height.saturating_sub(2).max(5);
    let inner = menu_surface(frame, centred(body, width, height));
    if inner.width < 6 || inner.height < 3 {
        return;
    }
    let content_width = inner.width as usize;

    let mut header = vec![
        Span::styled("• ", dim()),
        Span::styled("Proposed Plan", text_style().bold()),
    ];
    if review.feedback.is_some() {
        header.push(Span::styled("  (rejecting)", dim()));
    }
    let hint = Line::from(Span::styled(
        if review.feedback.is_some() {
            "type feedback · enter to reject · esc to go back"
        } else {
            "y to approve · n to reject · ↑↓ to scroll"
        },
        dim(),
    ));

    // Body rows left after the header, its blank line, the hint, and — while
    // rejecting — the feedback line.
    let reserved = 3 + u16::from(review.feedback.is_some());
    let rows = inner.height.saturating_sub(reserved).max(1) as usize;
    let plan = super::wrap_lines(
        super::render_markdown_at(&review.plan, content_width.saturating_sub(2).max(1)),
        content_width.saturating_sub(2).max(1),
    );
    let scroll = (review.scroll as usize).min(plan.len().saturating_sub(rows));

    let mut lines = vec![Line::from(header), Line::from("")];
    lines.extend(arm(
        plan.into_iter().skip(scroll).take(rows).collect(),
        OUTPUT_ARM,
    ));
    if let Some(feedback) = &review.feedback {
        let budget = content_width.saturating_sub(3);
        let shown: String = feedback
            .chars()
            .rev()
            .take(budget)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        lines.push(Line::from(vec![
            Span::styled("› ", text_style().bold()),
            Span::styled(shown, text_style()),
            Span::styled("▍", dim()),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(hint);
    lines.truncate(inner.height as usize);
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

/// The plan-mode interview.
///
/// `history_cell/request_user_input.rs:28-105` is the cell Codex renders a
/// question set as, and this borrows it whole: a `• ` bullet with a bold
/// `Questions` and an `n/m answered` counter dim after it, then each question
/// under a cyan-and-dim `  ↳ ` arm with its answer on a dim `    answer: `
/// continuation. An unanswered question is marked `(unanswered)`, dim.
fn draw_interview(frame: &mut Frame, app: &App, body: Rect) {
    let Some(interview) = &app.interview else {
        return;
    };
    let width = body.width.saturating_sub(6).clamp(24, 92);
    let height = body.height.saturating_sub(2).max(5);
    let inner = menu_surface(frame, centred(body, width, height));
    if inner.width < 6 || inner.height < 3 {
        return;
    }
    let content_width = inner.width as usize;
    let total = interview.questions.len();
    let answered = interview.current.min(total);

    let mut lines = vec![Line::from(vec![
        Span::styled("•", dim()),
        Span::raw(" "),
        Span::styled("Questions", text_style().bold()),
        Span::styled(format!(" {answered}/{total} answered"), dim()),
    ])];

    for (index, question) in interview.questions.iter().enumerate() {
        let mut rows = super::wrap_all(
            vec![Line::from(Span::styled(
                question.question.clone(),
                if index == interview.current {
                    text_style().bold()
                } else {
                    text_style()
                },
            ))],
            prefixed_block(content_width, ("  ↳ ", "    ")),
        );
        if index < interview.current {
            let answer = interview
                .answers
                .get(index)
                .map(String::as_str)
                .unwrap_or("");
            let answer = if answer.trim().is_empty() {
                "(skipped)".to_string()
            } else {
                answer.to_string()
            };
            rows.push(Line::from(vec![
                Span::styled("answer: ", dim()),
                Span::styled(answer, text_style()),
            ]));
        } else if index == interview.current {
            for (n, option) in question.options.iter().enumerate() {
                rows.push(Line::from(vec![
                    Span::styled(format!("{}) ", n + 1), accent()),
                    Span::styled(option.clone(), text_style()),
                ]));
            }
        } else if let Some(last) = rows.last_mut() {
            last.spans.push(Span::styled(" (unanswered)", dim()));
        }
        // `"  ↳ "` is cyan *and* dim upstream (`request_user_input.rs:102`),
        // both attributes on one span, which is why the arm reads as an accent
        // that has receded rather than as a second accent.
        lines.extend(super::prefix_rows(
            rows,
            "  ↳ ",
            "    ",
            accent().add_modifier(Modifier::DIM),
        ));
    }

    // The live answer line, and the keys that move it along.
    let budget = content_width.saturating_sub(3);
    let shown: String = interview
        .input
        .chars()
        .rev()
        .take(budget)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let tail = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("› ", text_style().bold()),
            Span::styled(shown, text_style()),
            Span::styled("▍", dim()),
        ]),
        Line::from(Span::styled(
            "1-9 to pick · enter for next · esc to skip",
            dim(),
        )),
    ];
    // The questions scroll under a fixed tail, so the input never leaves the
    // surface however many questions there are.
    let room = (inner.height as usize).saturating_sub(tail.len());
    let skip = lines.len().saturating_sub(room);
    let mut out: Vec<Line<'static>> = lines.into_iter().skip(skip).collect();
    out.extend(tail);
    frame.render_widget(Paragraph::new(Text::from(out)), inner);
}

// ---------------------------------------------------------------------------
// The session header (Codex's startup screen)
// ---------------------------------------------------------------------------

/// Draw `rows` inside Codex's card. `history_cell/session.rs:19-64`:
///
/// ```text
/// ╭ + "─".repeat(content + 2) + ╮
/// │ + " " + row padded to content + " " + │
/// ╰ + "─".repeat(content + 2) + ╯
/// ```
///
/// Every border glyph is dim, and so is the right-hand padding span — the card
/// is a frame around the content, never a thing to look at itself.
///
/// The card is sized to its *widest row*, not to the width it was allowed.
/// `with_border_internal` (`session.rs:34-42`) takes `forced_inner_width: None`
/// from `SessionHeaderHistoryCell::display_lines`, so `content_width` is the
/// max of the rows that came in; `card_inner_width` bounds what the rows may
/// contain (`session.rs:10-16`), never how wide the frame is drawn. A card
/// stretched to 56 columns around a 41-column row is the tell that gave this
/// away.
fn with_border(rows: Vec<Line<'static>>) -> Vec<Line<'static>> {
    let inner_width = rows.iter().map(Line::width).max().unwrap_or(0);
    let rule = "─".repeat(inner_width + 2);
    let mut out = vec![Line::from(Span::styled(format!("╭{rule}╮"), dim()))];
    for row in rows {
        let used = row.width();
        let mut spans = vec![Span::styled("│ ", dim())];
        spans.extend(row.spans);
        spans.push(Span::styled(
            " ".repeat(inner_width.saturating_sub(used)),
            dim(),
        ));
        spans.push(Span::styled(" │", dim()));
        out.push(Line::from(spans));
    }
    out.push(Line::from(Span::styled(format!("╰{rule}╯"), dim())));
    out
}

/// Codex's real startup screen: the session-header card, then the first-run
/// help block.
///
/// `history_cell/session.rs:312-390`. The rows are, in order: a `>_` banner
/// with the product name bold and its version dim, a blank row, then dim
/// `label:` keys padded to a common width with their values after them. The two
/// cells are separate parts of one `CompositeHistoryCell`, so exactly one blank
/// line separates the card from the help block (`base.rs:111-120`).
///
/// The commands listed are Wizard's, not Codex's. A screen that offered `/init`
/// and `/permissions` because Codex's does would be advertising commands this
/// agent does not have. The card's *rows* are Codex's exactly, though: upstream
/// carries `model:` and `directory:` and nothing else unless permissions are
/// wide open (`session.rs:369-376`), so nothing of Wizard's is added to it —
/// mode lives in the footer's status line, which is where Codex keeps that
/// class of fact too.
fn draw_session_header(frame: &mut Frame, app: &App, area: Rect) {
    if area.width < 8 || area.height == 0 {
        return;
    }
    // `card_inner_width`, `session.rs:10-16`: what the rows may *contain*. The
    // frame is then drawn around the widest row that came out, not around this.
    let inner_width = (area.width as usize)
        .saturating_sub(4)
        .min(SESSION_HEADER_MAX_INNER_WIDTH);
    // `len("directory:")`, the widest key. `session.rs:330-336`.
    let label_width = "directory:".len();
    let label = |key: &str| Span::styled(format!("{key:<label_width$} "), dim());

    let rows = vec![
        Line::from(vec![
            Span::styled(">_ ", dim()),
            Span::styled("Wizard", text_style().bold()),
            Span::styled(format!(" (v{})", env!("CARGO_PKG_VERSION")), dim()),
        ]),
        Line::from(""),
        Line::from(vec![
            label("model:"),
            super::model_span(app),
            Span::styled("   /model", accent()),
            Span::styled(" to change", dim()),
        ]),
        Line::from(vec![
            label("directory:"),
            Span::styled(
                // Saturating: the card is drawn from eight columns up, and the
                // key alone is ten wide, so a narrow terminal made this
                // subtraction underflow — a panic in a debug build, and a
                // nonsense budget in a release one.
                super::format_cwd(
                    &app.project_root,
                    inner_width.saturating_sub(label_width + 1),
                ),
                text_style(),
            ),
        ]),
    ];
    let rows: Vec<Line<'static>> = rows
        .into_iter()
        .map(|row| super::truncate_line(row, inner_width))
        .collect();

    let mut lines = with_border(rows);
    lines.push(Line::raw(""));

    // Anything that went wrong at startup, before the invitation to start: a
    // welcome screen that hid a broken provider to look tidier would be hiding
    // the one thing on it that needs acting on.
    let mut notices: Vec<String> = Vec::new();
    if let Some(err) = &app.provider_health_error {
        notices.push(format!("error: provider unreachable: {err}"));
    }
    notices.extend(
        app.transcript
            .iter()
            .filter_map(|item| match item {
                TranscriptItem::Notice(text) => text.lines().next().map(str::to_string),
                _ => None,
            })
            .rev()
            .take(3),
    );
    for notice in &notices {
        lines.extend(notice_cell(notice, area.width as usize));
    }
    if !notices.is_empty() {
        lines.push(Line::raw(""));
    }

    lines.push(Line::from(Span::styled(
        "  To get started, describe a task or try one of these commands:",
        dim(),
    )));
    lines.push(Line::raw(""));
    for (command, blurb) in [
        ("/model", " - choose the model and provider"),
        ("/ui", " - wear another agent's chrome"),
        ("/fusion", " - answer every turn with a panel of models"),
        ("/ultra", " - fan the turn out over N drafting candidates"),
        ("/publish", " - publish this project"),
        ("/help", " - every command and key"),
    ] {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(command, text_style()),
            Span::styled(blurb, dim()),
        ]));
    }
    lines.truncate(area.height as usize);
    frame.render_widget(Paragraph::new(Text::from(lines)), area);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(width: u16, height: u16) -> Rect {
        Rect::new(0, 0, width, height)
    }

    /// The plain text of a rendered line, which is what a screenshot shows.
    fn flat(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn tool(name: &str, args: serde_json::Value, output: Option<&str>) -> ToolItem {
        ToolItem {
            name: name.to_string(),
            args,
            call_id: String::new(),
            output: output.map(|content| crate::transcript::ToolItemOutput {
                content: content.to_string(),
                is_error: false,
            }),
            progress: String::new(),
        }
    }

    #[test]
    fn every_wizard_tool_lands_on_a_codex_cell() {
        // The mapping the whole transcript rests on. A tool that fell through
        // to `Call` when it had a purpose-built cell would still render, which
        // is exactly why this is asserted rather than eyeballed.
        for (name, args, expected) in [
            ("execute", serde_json::json!({"command": "ls"}), Shape::Exec),
            (
                "execute",
                serde_json::json!({"command": "make", "run_in_background": true}),
                Shape::Background,
            ),
            ("task_output", serde_json::Value::Null, Shape::Background),
            ("task_kill", serde_json::Value::Null, Shape::Background),
            ("read_file", serde_json::Value::Null, Shape::Explore),
            ("list_files", serde_json::Value::Null, Shape::Explore),
            ("search_files", serde_json::Value::Null, Shape::Explore),
            ("write_file", serde_json::Value::Null, Shape::Patch),
            ("edit_file", serde_json::Value::Null, Shape::Patch),
            ("spawn_subagent", serde_json::Value::Null, Shape::Agent),
            // Everything else — MCP servers, `web_search`, a loadout's own
            // tools — takes the cell Codex uses for a tool it does not know
            // the shape of.
            ("web_search", serde_json::Value::Null, Shape::Call),
            ("some_mcp_tool", serde_json::Value::Null, Shape::Call),
        ] {
            assert_eq!(Shape::of(&tool(name, args, None)), expected, "{name}");
        }
    }

    #[test]
    fn the_session_card_is_drawn_around_its_widest_row_not_the_width_it_was_given() {
        // `with_border_internal(lines, None)` — `history_cell/session.rs:34-42`.
        // `card_inner_width` bounds what a row may *contain*; the frame is then
        // sized to what actually came out. Getting this backwards stretches the
        // card to 56 columns around a 40-column row, which is the single most
        // visible way this skin can stop looking like Codex.
        let rows = vec![Line::from("short"), Line::from("a considerably longer row")];
        let card = with_border(rows);
        let widest = "a considerably longer row".width();
        assert_eq!(card[0].width(), widest + 4, "top rule hugs the widest row");
        for row in &card {
            assert_eq!(row.width(), widest + 4, "every row is the same width");
        }
        assert_eq!(flat(&card[1]), format!("│ short{} │", " ".repeat(20)));
    }

    #[test]
    fn an_exec_cell_reads_as_bullet_verb_command() {
        let lines = tool_cell(
            &tool(
                "execute",
                serde_json::json!({"command": "cargo test -p wizard"}),
                Some("ok"),
            ),
            false,
            0,
            80,
        );
        assert_eq!(flat(&lines[0]), "• Ran cargo test -p wizard");
        assert_eq!(flat(&lines[1]), "  └ ok");

        // Still in flight: the present-tense verb, and no arm yet.
        let running = tool_cell(
            &tool("execute", serde_json::json!({"command": "sleep 1"}), None),
            false,
            0,
            80,
        );
        assert_eq!(
            flat(&running[0]).trim_start_matches(['•', '◦', ' ']),
            "Running sleep 1"
        );
        assert_eq!(running.len(), 1);
    }

    #[test]
    fn an_exploring_cell_lists_what_it_touched_and_nothing_else() {
        // The `Read`/`Search`/`List` rows *are* the summary; hanging a file's
        // contents under them would make `Explored` the tallest cell on screen.
        let read = tool_cell(
            &tool(
                "read_file",
                serde_json::json!({"path": "src/wrapping.rs"}),
                Some("a thousand lines of source"),
            ),
            false,
            0,
            80,
        );
        assert_eq!(flat(&read[0]), "• Explored ");
        assert_eq!(flat(&read[1]), "  └ Read src/wrapping.rs");
        assert_eq!(read.len(), 2, "no output block: {read:?}");

        let search = tool_cell(
            &tool(
                "search_files",
                serde_json::json!({"pattern": "shimmer", "path": "src"}),
                Some(""),
            ),
            false,
            0,
            80,
        );
        assert_eq!(flat(&search[1]), "  └ Search shimmer in src");
    }

    #[test]
    fn a_folded_card_keeps_its_arm_and_says_what_is_under_it() {
        // Folding is Wizard's own idea (a click, or Ctrl-T). Rendering it as a
        // header with the output silently gone would look like a cell that
        // produced nothing, so it takes the elision's grammar instead.
        let lines = tool_cell(
            &tool(
                "execute",
                serde_json::json!({"command": "ls"}),
                Some("a\nb\nc"),
            ),
            true,
            0,
            80,
        );
        assert_eq!(lines.len(), 2);
        assert_eq!(flat(&lines[1]), "  └ … +3 lines (ctrl + t to expand)");
    }

    #[test]
    fn a_background_terminal_says_whether_it_started_or_reached_into_one() {
        // `history_cell/exec.rs:30-34`: a cell that started something keeps the
        // bullet; one that reached into something already running leads with a
        // dim `↳ ` instead.
        let started = tool_cell(
            &tool(
                "execute",
                serde_json::json!({"command": "make -j8", "run_in_background": true}),
                Some("task 3"),
            ),
            false,
            0,
            80,
        );
        assert!(
            flat(&started[0]).ends_with("Started background terminal · make -j8"),
            "{}",
            flat(&started[0])
        );
        let touched = tool_cell(
            &tool("task_output", serde_json::Value::Null, Some("still going")),
            false,
            0,
            80,
        );
        assert_eq!(flat(&touched[0]), "↳ Interacted with background terminal");
    }

    #[test]
    fn a_user_message_and_the_composer_occupy_the_same_columns() {
        // The reason the user cell wraps at `width - 3` and not `width - 2`:
        // a submitted message has to land where it was typed.
        let long = "x".repeat(200);
        let lines = user_cell(&long, 40);
        for line in &lines[1..lines.len() - 1] {
            assert!(line.width() <= 40, "{}", line.width());
        }
        let typed = super::super::wrap_rows(&long.chars().collect::<Vec<_>>(), composer_budget(40));
        let committed = lines.len() - 2;
        assert_eq!(committed, typed.len(), "same number of rows either side");
    }

    #[test]
    fn the_shortcut_overlay_is_two_padded_columns_and_a_trailer() {
        let app = crate::app::App::new(crate::config::Config::default());
        let lines = shortcut_overlay_lines(&app);
        // A blank line and the `/help` trailer close it out.
        assert_eq!(flat(&lines[lines.len() - 2]), "");
        assert!(flat(&lines[lines.len() - 1]).ends_with("/help"));
        // Column 1 starts at the same column on every entry row.
        let starts: Vec<usize> = lines[..lines.len() - 2]
            .iter()
            .filter(|line| line.spans.len() == 3)
            .map(|line| line.spans[0].content.width() + line.spans[1].content.width())
            .collect();
        assert!(starts.len() >= 2);
        assert!(
            starts.windows(2).all(|pair| pair[0] == pair[1]),
            "ragged columns: {starts:?}"
        );
    }

    #[test]
    fn the_footer_grows_only_for_the_shortcut_overlay() {
        let mut app = crate::app::App::new(crate::config::Config::default());
        assert_eq!(FooterMode::of(&app), FooterMode::ComposerEmpty);
        assert_eq!(footer_height(&app), 1);

        app.input = "hello".to_string();
        assert_eq!(FooterMode::of(&app), FooterMode::ComposerHasDraft);
        assert_eq!(footer_height(&app), 1);

        app.ctrl_c_armed = true;
        assert_eq!(FooterMode::of(&app), FooterMode::QuitShortcutReminder);

        // `?` is the gesture Codex binds the overlay to, and here it is a draft
        // no key handler consumes — so the hint that names it is true.
        app.ctrl_c_armed = false;
        app.input = "?".to_string();
        assert_eq!(FooterMode::of(&app), FooterMode::ShortcutOverlay);
        assert!(footer_height(&app) > 1);
        // And the layout gives it those rows rather than clipping it.
        let regions = regions(&app, rect(80, 30));
        assert_eq!(regions.footer.height, footer_height(&app));
        assert_eq!(regions.footer.bottom(), 30);
    }

    #[test]
    fn elapsed_is_compact_in_codexs_exact_forms() {
        // The table at `status_indicator_widget.rs:313-325`, verbatim.
        for (secs, expected) in [
            (0, "0s"),
            (1, "1s"),
            (59, "59s"),
            (60, "1m 00s"),
            (61, "1m 01s"),
            (185, "3m 05s"),
            (3599, "59m 59s"),
            (3600, "1h 00m 00s"),
            (3661, "1h 01m 01s"),
            (90_123, "25h 02m 03s"),
        ] {
            assert_eq!(fmt_elapsed_compact(secs), expected, "{secs}s");
        }
    }

    #[test]
    fn a_duration_has_no_hour_unit_and_two_decimals_under_a_minute() {
        // `codex-rs/utils/elapsed/src/lib.rs:9-24` — deliberately a *different*
        // formatter from the status row's, which is why both exist.
        assert_eq!(format_duration(0, 250), "250ms");
        assert_eq!(format_duration(1, 1500), "1.50s");
        assert_eq!(format_duration(59, 59_999), "60.00s");
        assert_eq!(format_duration(75, 75_000), "1m 15s");
        assert_eq!(format_duration(3600, 3_600_000), "60m 00s");
    }

    #[test]
    fn the_composer_leaves_two_columns_of_prompt_and_one_of_right_margin() {
        // The number this whole layout rests on: a submitted message has to
        // land in exactly the columns it was typed in, so the composer's
        // budget and the user cell's wrap width must be the same `width - 3`.
        assert_eq!(composer_budget(80), 77);
        assert_eq!(composer_budget(4), 1);
        // Never zero, however narrow the terminal gets.
        assert_eq!(composer_budget(1), 1);
        assert_eq!(composer_budget(0), 1);

        let lines = user_cell("hello", 80);
        assert_eq!(lines.len(), 3, "a blank row above and below the message");
        let body: String = lines[1]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(body, "› hello");
    }

    #[test]
    fn a_gutter_is_subtracted_before_the_wrap_not_after() {
        // `PrefixedBlock::wrap_width` uses the *wider* of the two prefixes, so
        // `"  └ "` and `"    "` both cost four columns and a prefixed row can
        // never overhang the right edge.
        assert_eq!(prefixed_block(80, OUTPUT_ARM), 76);
        assert_eq!(prefixed_block(80, COMMAND_ARM), 76);
        assert_eq!(prefixed_block(2, OUTPUT_ARM), 1);
    }

    #[test]
    fn output_is_elided_from_the_middle_with_the_head_and_tail_kept() {
        let rows: Vec<Line<'static>> = (1..=10)
            .map(|n| Line::from(Span::raw(n.to_string())))
            .collect();
        let out = truncate_rows_middle(rows, TOOL_CALL_MAX_LINES);
        let text: Vec<String> = out
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        // Five rows: two head, the elision, two tail — `available / 2` head and
        // the remainder to the tail (`render.rs:575-577`).
        assert_eq!(out.len(), TOOL_CALL_MAX_LINES);
        assert_eq!(text[0], "1");
        assert_eq!(text[1], "2");
        assert!(text[2].starts_with("… +6 lines ("), "{:?}", text[2]);
        assert_eq!(text[3], "9");
        assert_eq!(text[4], "10");
        // Under the budget, nothing is touched.
        let short: Vec<Line<'static>> = (1..=3)
            .map(|n| Line::from(Span::raw(n.to_string())))
            .collect();
        assert_eq!(truncate_rows_middle(short, TOOL_CALL_MAX_LINES).len(), 3);
    }

    #[test]
    fn a_wrapped_command_is_capped_at_two_rows_with_a_bare_elision() {
        // The command elision carries no transcript hint — that is the whole
        // difference between `ellipsis_line` and `output_ellipsis_text`.
        let command = (1..=8)
            .map(|n| format!("echo line-{n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let (_, rest) = command_display_lines(&command, 40, 6);
        assert_eq!(rest.len(), COMMAND_CONTINUATION_MAX_LINES + 1);
        let last: String = rest[COMMAND_CONTINUATION_MAX_LINES]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(last.contains("… +"), "{last}");
        assert!(!last.contains("ctrl"), "no transcript hint here: {last}");
        for line in &rest {
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            assert!(text.starts_with("  │ "), "{text}");
        }
    }

    #[test]
    fn the_footer_collapses_in_codexs_order() {
        // The observed sequence from `…__footer_collapse_plan_*`, with the
        // right-hand context standing in for the context window.
        let mode = Some("Plan mode");
        let context = "98% context left".width() as u16;
        let render = |width: u16, hint: HintKind, cycle: bool| {
            let (left, show) =
                single_line_footer_layout(rect(width, 1), context, mode, cycle, hint);
            let text = match left {
                SummaryLeft::Line(line) => line.spans.iter().map(|s| s.content.as_ref()).collect(),
                SummaryLeft::None => String::new(),
            };
            (text, show)
        };

        // Wide: the fullest left line and the context together.
        let (text, show) = render(120, HintKind::QueueMessage, false);
        assert_eq!(text, "enter to queue message · Plan mode");
        assert!(show);
        // Narrow enough that the message hint has to shorten, but the queue
        // hint is kept over the context.
        let (text, show) = render(50, HintKind::QueueMessage, false);
        assert_eq!(text, "enter to queue · Plan mode");
        assert!(show);
        // Narrower still: the context goes, the full hint comes back.
        let (text, show) = render(40, HintKind::QueueMessage, false);
        assert_eq!(text, "enter to queue message · Plan mode");
        assert!(!show);
        let (text, _) = render(30, HintKind::QueueMessage, false);
        assert_eq!(text, "enter to queue · Plan mode");
        // Too narrow for any hint: the mode label alone.
        let (text, _) = render(20, HintKind::QueueMessage, false);
        assert_eq!(text, "Plan mode");
        // And nothing at all fits when even that cannot.
        let (text, _) = render(6, HintKind::QueueMessage, false);
        assert_eq!(text, "");
    }

    #[test]
    fn the_context_never_outlives_the_cycle_hint() {
        // The anti-flicker rule (`context_requires_cycle_hint`,
        // `footer.rs:390`): outside queue mode, if `(shift+tab to cycle)`
        // cannot fit alongside the context then the context is dropped too,
        // rather than the right side surviving one resize step longer.
        let mode = Some("Plan mode");
        let context = "12.3k tok used".width() as u16;
        let (_, show) = single_line_footer_layout(
            rect(46, 1),
            context,
            mode,
            /*cycle*/ true,
            HintKind::Shortcuts,
        );
        assert!(!show, "the context goes when the cycle hint cannot fit");
        // With the cycle hint switched off there is nothing to outlive, so the
        // same width keeps the context.
        let (_, show) = single_line_footer_layout(
            rect(46, 1),
            context,
            mode,
            /*cycle*/ false,
            HintKind::Shortcuts,
        );
        assert!(show);
    }

    #[test]
    fn the_right_side_is_padded_two_columns_off_the_edge() {
        // `right_aligned_x`, `footer.rs:601-622`: 80 columns, a 17-column
        // context, two columns of right padding — the first character lands at
        // column 61.
        assert_eq!(right_aligned_x(rect(80, 1), 17), Some(61));
        // Content wider than the room available is pinned to the indent
        // instead of running off the left edge.
        assert_eq!(right_aligned_x(rect(20, 1), 40), Some(2));
        assert_eq!(right_aligned_x(rect(0, 1), 4), None);
        assert_eq!(right_aligned_x(rect(80, 1), 0), None);
    }

    #[test]
    fn the_bottom_pane_is_five_rows_while_a_turn_runs_and_four_when_idle() {
        // Idle: three composer rows and one footer row, with no spacing
        // between them (`FOOTER_SPACING_HEIGHT == 0`). Running: a status row
        // and the bare separator line above those.
        let app = crate::app::App::new(crate::config::Config::default());
        let idle = regions(&app, rect(80, 24));
        assert_eq!(idle.status.height, 0);
        assert_eq!(idle.composer.height, 3);
        assert_eq!(idle.footer.height, 1);
        assert_eq!(
            idle.footer.y,
            idle.composer.bottom(),
            "the footer sits immediately under the composer"
        );

        let mut busy = crate::app::App::new(crate::config::Config::default());
        busy.status.busy = true;
        let running = regions(&busy, rect(80, 24));
        assert!(running.status.height >= 1);
        assert_eq!(
            running.composer.y,
            running.status.bottom() + 1,
            "exactly one bare separator line between the two"
        );
    }
}
