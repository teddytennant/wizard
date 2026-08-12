//! Grok Build's chrome: xAI's coding-agent TUI, worn by Wizard.
//!
//! Ported from the `xai-grok-pager` and `xai-grok-pager-render` crates of
//! <https://github.com/xai-org/grok-build> (Apache-2.0). Throughout, `P/` is
//! `crates/codegen/xai-grok-pager/` and `R/` is
//! `crates/codegen/xai-grok-pager-render/`; every function and constant that
//! came from up there names the file it came from. Wizard is MIT; see
//! `docs/ui-skins.md` for the attribution in full.
//!
//! **The one idea.** Every scrollback entry is a `│A│PL│content│PR│` block: one
//! column of accent rail, two of left padding, the content, two held back at
//! the right (`P/src/scrollback/layout.rs:41-56`). The rail is painted down the
//! block's *whole* height — vertical padding included — and its colour is the
//! block's entire status readout. Nothing else in the screen has a border
//! except the composer, and that is painted cell by cell rather than with a
//! `ratatui::widgets::Block`, because the info line has to sit *inside* its
//! bottom edge.
//!
//! **What this is not.** It is Wizard wearing Grok Build's chrome, not a clone.
//! The commands are Wizard's, the state on screen is Wizard's — genie/sovereign
//! mode, `ULTRA ×N`, fusion, background subagents and tasks, the context meter,
//! the queued-message count, `/diff`, the todo band — and where Grok Build has
//! a block shaped like one of those (it has real subagent and background-task
//! rows) Wizard's is mapped onto it rather than something new being invented.
//!
//! **Two house rules override fidelity**, and both are called out again where
//! they bite:
//!
//! 1. *Meaning never rests on hue.* Grok Build carries tool status in colour
//!    alone — the `◆` bullet and the `┃` rail go red, green or violet and no
//!    glyph changes ([`tool_bullet`]). Wizard keeps `✗` for failure, so a
//!    16-colour terminal and `NO_COLOR` still say which call broke.
//! 2. *Tokens, never colours.* Nothing here names a [`ratatui::style::Color`];
//!    every hue is a [`Token`] the active theme resolves. The exception the
//!    house rules already carve out is [`crate::skin::blend`], whose [`Tint`]
//!    blends against the terminal's own background when the environment
//!    reports one and otherwise reads `bg.raised` / `bg.sunken` off the theme —
//!    that is what the prompt band and the tool-output panels are made of.
//!
//! **What Wizard cannot reach.** Upstream's scrollback has a *selection* (one
//! entry is current, arrow keys move it) and a *fold cycle* per entry
//! (`Collapsed → Truncated → Expanded`). Wizard has neither: the only fold
//! state in [`crate::app::TranscriptView`] is a boolean on tool rows. So every
//! display mode a user could not leave again is rendered one step more open
//! than upstream defaults to — see [`Mode`] — because a collapsed block nobody
//! can expand is not a fold, it is deletion.
//!
//! The other thing out of reach is **timestamps**. Upstream overlays a
//! `  h:mm AM/PM` on the right of a user or agent message's first content row,
//! reserving 10 columns off the wrap width for it
//! (`P/src/scrollback/wrappers/entry_renderer.rs:371-388`, `:931-958`).
//! [`crate::transcript::TranscriptItem`] records no creation time, and the only
//! clock a renderer can reach is "now" — which would stamp every message of a
//! resumed session with the moment it was redrawn. So the gutter is not
//! reserved and nothing is overlaid: a wrong timestamp is worse than none, and
//! the alternative needs a field on the transcript model, not on this file.

use std::time::Duration;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Margin, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState};
use unicode_width::UnicodeWidthStr;

use crate::app::{App, PaneStatus, SubagentPane, TranscriptView};
use crate::image_view::{ImageBlock, ImageCache};
use crate::skin::blend::{self, Tint};
use crate::skin::motion;
use crate::theme::{self, ColorDepth, Token};
use crate::tools::tasks::{Task, TaskStatus};
use crate::tools::todo::{TodoItem, TodoStatus};
use crate::transcript::{ToolItem, TranscriptItem};

use super::RowTag;

// ---------------------------------------------------------------------------
// Geometry
// ---------------------------------------------------------------------------

/// The accent column. One cell, always reserved even by the blocks that paint
/// nothing into it, so every block's prose starts in the same column.
///
/// Ported from `P/src/scrollback/layout.rs:38` (xai-org/grok-build, Apache-2.0).
const ACCENT: u16 = 1;

/// Columns between the rail and the content, and between the content and the
/// block's right edge.
///
/// Ported from `R/src/appearance/config.rs:212-213` (xai-org/grok-build,
/// Apache-2.0). The right pad really is 2: the doc comment at
/// `P/src/scrollback/layout.rs:20` says 1 and so does the constant explicitly
/// labelled "legacy" at `entry_renderer.rs:369`, but both `LayoutConfig` and
/// `RawLayoutConfig` default it to 2 and `chrome_width()` reads it from there.
const PAD_LEFT: u16 = 2;
const PAD_RIGHT: u16 = 2;

/// Everything a block spends before its first character of prose, plus what it
/// holds back after the last: `1 + 2 + 2`.
///
/// Ported from `P/src/scrollback/wrappers/entry_renderer.rs:357-365`.
const CHROME_WIDTH: u16 = ACCENT + PAD_LEFT + PAD_RIGHT;

/// The screen's own margin, outside the block layout.
///
/// Ported from `R/src/appearance/config.rs:206-211`. The scrollbar lives one
/// column further out still (`P/src/views/agent.rs:363`: `area.right() - 1`),
/// which is why the right margin is 2 but the content stops 3 columns short.
const OUTER_VPAD: u16 = 1;
const OUTER_HPAD: u16 = 2;

/// Below this the screen drops its outer margins and its gap rows: they are the
/// first thing worth spending when a terminal has nothing left to spend, and a
/// composer squeezed out of existence is worse than an unbalanced margin.
const COMPACT_HEIGHT: u16 = 14;

// ---------------------------------------------------------------------------
// Glyphs
// ---------------------------------------------------------------------------

/// Ported from `R/src/glyphs.rs` (xai-org/grok-build, Apache-2.0). The legacy
/// ConHost fallbacks upstream carries (`│` for the rail, `♦` for the diamond)
/// are not reproduced: Wizard has no console-generation probe to switch on, and
/// every terminal it supports draws these.
const RAIL: &str = "\u{2503}"; // ┃  accent_bar()
const RAIL_COLLAPSED: &str = "\u{2759}"; // ❙  collapsed_accent()
const DIAMOND: &str = "\u{25c6}"; // ◆  diamond_filled()
const PROMPT_ARROW: &str = "\u{276f} "; // ❯  prompt_arrow(), width 2
const TOKEN_ARROW: &str = "\u{21e3}"; // ⇣  token_arrow()

/// The turn spinner. Upstream's braille wheel, *not* the four half-circles the
/// skin table carries for `grok` — that table is shared with the parts of the
/// TUI this file does not own, and it is not editable from here.
///
/// Ported from `R/src/glyphs.rs:226-229`.
const SPINNER: [&str; 8] = [
    "\u{280b}", "\u{2819}", "\u{2839}", "\u{2838}", "\u{283c}", "\u{2834}", "\u{2826}", "\u{2827}",
];

/// The idle "something is still running in the background" breath.
///
/// Ported from `R/src/glyphs.rs:149`.
const MONITOR: [&str; 4] = ["\u{25cb}", "\u{25ce}", "\u{25c9}", "\u{25ce}"];

/// The background-task chip's spinner on the top status bar.
///
/// Ported from `R/src/glyphs.rs:247-249`.
const DOTS: [&str; 4] = ["\u{22c5}", ":", "\u{2e2c}", "\u{2059}"];

// ---------------------------------------------------------------------------
// Animation
// ---------------------------------------------------------------------------

/// Upstream animates at 30 fps (`R/src/appearance/config.rs:398-404`); Wizard's
/// event loop ticks at 10 Hz (`crate::app::runtime`, a 100 ms interval). Every
/// speed and divisor below is therefore upstream's value rescaled by 3 so the
/// *wall-clock* cadence matches — the thing a person can actually see. Porting
/// the raw numbers would have run every animation at a third of its speed.
const TICK_RATIO: f32 = 3.0;

/// Speed of the wave travelling down a running block's rail.
///
/// Ported from `P/src/scrollback/wrappers/entry_renderer.rs:23`
/// (`WAVE_SPEED = 0.15` at 30 fps).
const WAVE_SPEED: f32 = 0.15 * TICK_RATIO;

/// Rows per wavelength, so a tall block ripples rather than blinking in unison.
///
/// Ported from `R/src/appearance/config.rs:400` (`wave_rows: 32`).
const WAVE_ROWS: u16 = 32;

/// Speed of the "waiting on you" diamond's pulse.
///
/// Ported from `P/src/views/turn_status.rs:48`
/// (`USER_WAITING_PULSE_SPEED = 0.08` at 30 fps).
const PULSE_SPEED: f32 = 0.08 * TICK_RATIO;

/// Ticks each spinner frame is held. Upstream holds 4 frames at 30 fps
/// (~133 ms, `P/src/views/turn_status.rs:32`); at 10 Hz one tick is 100 ms,
/// which is as close as this clock divides.
const SPINNER_DIVISOR: u64 = 1;

/// The idle watcher breath runs at half the spinner's rate
/// (`P/src/views/turn_status.rs:38`, `MONITOR_PULSE_DIVISOR = 8`).
const MONITOR_DIVISOR: u64 = 3;

/// How far a collapsed, groupable block's rail is blended toward the
/// background: half way.
///
/// Ported from `R/src/appearance/config.rs:177` (`dim_accent: 0.5`).
const DIM_ACCENT: f32 = 0.5;

// ---------------------------------------------------------------------------
// Truncation windows
// ---------------------------------------------------------------------------

/// A command's output preview: the first two lines, the marker, the last three.
///
/// Ported from `R/src/appearance/config.rs:716-717`.
const EXECUTE_FIRST: usize = 2;
const EXECUTE_LAST: usize = 3;

/// A file read's preview window.
///
/// Ported from `P/src/scrollback/blocks/tool/read.rs:18-19`.
const READ_FIRST: usize = 5;
const READ_LAST: usize = 3;

/// Every other tool's inline output cap, and what survives once it is over.
///
/// Ported from `P/src/scrollback/blocks/tool/use_tool.rs` (`MAX_INLINE_LINES`,
/// `TRUNCATED_INLINE_LINES`).
const MAX_INLINE: usize = 10;
const TRUNCATED_INLINE: usize = 3;

/// Wrapped lines a truncated thinking block keeps, counted from the end.
///
/// Ported from `R/src/appearance/config.rs:585` (`truncated_lines: 3`).
const THINKING_TAIL: usize = 3;

/// Columns the collapsed one-line tool summary is hard-cut at, matching the
/// house transcript's own argument budget so the two skins summarise a call
/// the same way.
const SUMMARY_WIDTH: usize = 64;

// ---------------------------------------------------------------------------
// Colour helpers
// ---------------------------------------------------------------------------

/// `token`'s colour blended `amount` of the way toward the terminal's own
/// background — upstream's `blend_color(bg, color, 1.0 - amount)`.
///
/// Ported in spirit from `R/src/render/color.rs:198-206`, which likewise gives
/// up and returns the colour unchanged when either end is not RGB (an indexed
/// or named colour is whatever the user's palette says it is, so there is
/// nothing to interpolate). Where upstream would then simply look undimmed,
/// this adds `Modifier::DIM` instead, so the low-colour path still separates a
/// collapsed block from an open one — the house rule that meaning survives
/// palette loss, applied to the one place upstream lets it rest on hue.
fn fade(token: Token, amount: f32) -> Style {
    let color = theme::color(token);
    let ends = (theme::active().depth() == ColorDepth::TrueColor)
        .then(|| Some((rgb(color)?, blend::terminal_bg()?)))
        .flatten();
    match ends {
        Some((fg, bg)) => {
            let (r, g, b) = blend::blend(fg, bg, 1.0 - amount.clamp(0.0, 1.0));
            Style::default().fg(Color::Rgb(r, g, b))
        }
        None => Style::default().fg(color).add_modifier(Modifier::DIM),
    }
}

/// A colour as RGB, when it is one.
fn rgb(color: Color) -> Option<(u8, u8, u8)> {
    match color {
        Color::Rgb(r, g, b) => Some((r, g, b)),
        _ => None,
    }
}

/// `style` with a background, when the block has one.
fn on(style: Style, bg: Option<Color>) -> Style {
    match bg {
        Some(color) => style.bg(color),
        None => style,
    }
}

/// `n` blank columns carrying a background.
fn blanks(n: usize, bg: Option<Color>) -> Span<'static> {
    Span::styled(" ".repeat(n), on(Style::default(), bg))
}

// ---------------------------------------------------------------------------
// The block model
// ---------------------------------------------------------------------------

/// What an entry is, which is what decides its chrome.
///
/// The variants are upstream's `RenderBlock` (`P/src/scrollback/block.rs:371`),
/// minus the ones Wizard has no state for (credit-limit cards, workflow rows,
/// `/context` cards) and with `Notice` standing in for `SystemMessageBlock`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// The user's echoed prompt. The *only* block with vertical padding and a
    /// background — everything else overrides both away
    /// (`P/src/scrollback/blocks/user.rs:484-492`).
    UserPrompt,
    /// An assistant message. No rail at all: the column is reserved and left
    /// blank (`P/src/scrollback/blocks/agent.rs:208`).
    Agent,
    /// Model reasoning (`P/src/scrollback/blocks/thinking.rs`).
    Thinking,
    /// A tool call and its output (`P/src/scrollback/blocks/tool/`).
    Tool,
    /// A delegated subagent, one row (`P/src/scrollback/blocks/subagent.rs`).
    Subagent,
    /// A detached background command, one row
    /// (`P/src/scrollback/blocks/bg_task.rs`).
    BgTask,
    /// A system notice (`P/src/scrollback/blocks/system.rs`).
    System,
}

impl Kind {
    /// Whether adjacent collapsed entries of this kind pack with no gap row.
    ///
    /// Ported from the `is_groupable` overrides listed at
    /// `P/src/scrollback/block.rs:185` and its implementors: true for tools,
    /// thinking, system, subagent and bg-task rows; false (the trait default)
    /// for user prompts and agent messages.
    fn groupable(self) -> bool {
        !matches!(self, Kind::UserPrompt | Kind::Agent)
    }

    /// One blank styled row above the content and one below.
    ///
    /// Ported from `P/src/scrollback/wrappers/entry_renderer.rs:496-505`. Only
    /// the user prompt returns true, which is what turns its tint into a band
    /// with a margin rather than a highlighted paragraph.
    fn vpad(self) -> u16 {
        u16::from(self == Kind::UserPrompt)
    }

    /// The slab behind the whole entry, pads and padding rows included.
    ///
    /// Ported from `P/src/scrollback/blocks/user.rs:484-490`: `UserPromptBlock`
    /// is the only block with a non-`None` `background()`, and the only one
    /// whose `accent_background()` is true ("fill accent column with block bg
    /// so it matches content"). Upstream names `theme.bg_light`; here it is a
    /// [`Tint`], which blends against the terminal's own background when the
    /// environment reports one and otherwise takes the colour the theme
    /// declares — `bg.raised`, set in `assets/themes/grok.toml` to GrokNight's
    /// `bg_light`. See [`Tint::resolve`].
    fn tint(self) -> Option<Tint> {
        (self == Kind::UserPrompt).then_some(Tint::Raised)
    }
}

/// How much of an entry is on screen.
///
/// Upstream's `DisplayMode` (`P/src/scrollback/state/types.rs`). Wizard reaches
/// two of the three: `Collapsed` is a folded tool row (Ctrl-T, or a click on
/// the header) and every subagent/bg-task row, which upstream also pins to one
/// line for good. `Truncated` is a preview with a marker in the middle.
/// `Expanded` is everything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Collapsed,
    Truncated,
    Expanded,
}

/// One scrollback entry, laid out and ready for [`decorate`].
struct Entry {
    kind: Kind,
    mode: Mode,
    /// The rail's colour, or `None` for a block that paints no rail. `None` is
    /// not the same as "leave the cell alone" — the column is still *cleared*,
    /// which is why this is an `Option<Token>` and not an absent rail
    /// (`P/src/scrollback/wrappers/entry_renderer.rs:802-807`).
    accent: Option<Token>,
    /// The rail and the bullet breathe while the block is working.
    animated: bool,
    /// The agent is blocked on the user, so the wave freezes into a solid bar
    /// at full colour — "paused on you" rather than "loading"
    /// (`entry_renderer.rs:813-820`).
    pending: bool,
    /// The `◆` leading the first content row, and the token it takes its
    /// colour from. `None` for blocks that have no bullet.
    bullet: Option<(&'static str, Token)>,
    /// Content rows, already wrapped to the block's content width.
    rows: Vec<Row>,
    /// This entry's index in the transcript, when clicking its header folds it.
    card: Option<usize>,
    /// `(row offset, image slot, rows)` for any image block inside `rows`.
    images: Vec<(usize, usize, u16)>,
}

/// One content row, and whether it sits on a panel band.
///
/// Panel bands are per-*line* backgrounds (`P/src/scrollback/types.rs:144-151`)
/// rather than block backgrounds: a tool's output preview is `bg_dark` behind
/// those rows only, and the rows above and below it are not.
struct Row {
    line: Line<'static>,
    panel: bool,
}

impl Row {
    fn plain(line: Line<'static>) -> Row {
        Row { line, panel: false }
    }

    fn panel(line: Line<'static>) -> Row {
        Row { line, panel: true }
    }
}

/// The gap row between two entries.
///
/// Ported from `P/src/scrollback/state/layout.rs:1579-1587`: two *groupable*
/// entries that are both *collapsed* pack solid, so a run of folded tool calls
/// reads as one group; everything else gets one blank row. This is the rule the
/// `┃` → `❙` switch goes with — a run that packs would otherwise merge its
/// rails into one unbroken bar, which is exactly what the thinner glyph and the
/// half-brightness are there to prevent.
fn gap_after(above: &Entry, below: &Entry) -> u16 {
    let groupable = above.kind.groupable() && below.kind.groupable();
    let collapsed = above.mode == Mode::Collapsed && below.mode == Mode::Collapsed;
    u16::from(!(groupable && collapsed))
}

/// Paint one entry: the accent column down its full height, the pads, the
/// bullet, the content, and the slab carried to the block's right edge.
///
/// Ported from `P/src/scrollback/wrappers/entry_renderer.rs:715-857`, which
/// does the same thing cell by cell into a `Buffer`. Here it produces `Line`s,
/// because Wizard's transcript is a scrolled `Paragraph` rather than a windowed
/// pane, and rows have to survive being sliced by the scroll offset.
fn decorate(entry: &Entry, width: u16, tick: u64) -> Vec<Line<'static>> {
    let content_width = content_width(width) as usize;
    let bg = entry.kind.tint().and_then(Tint::resolve);
    let vpad = entry.kind.vpad();
    let total = entry.rows.len() as u16 + vpad * 2;

    let mut out: Vec<Line<'static>> = Vec::with_capacity(total as usize);
    let mut push = |content: Vec<Span<'static>>, row: u16, panel: bool, bullet: bool| {
        // A panel band is a per-line background that overrides the block's,
        // and it covers the content columns only — the rail and the pads stay
        // on whatever the block itself sits on. Upstream paints these
        // `theme.bg_dark` (`.with_panel_background(...)`); [`Tint::Sunken`] is
        // that same step *away* from the background, resolved through
        // `bg.sunken` when the terminal reports nothing to blend against.
        let line_bg = if panel {
            Tint::Sunken.resolve().or(bg)
        } else {
            bg
        };
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(content.len() + 5);
        spans.push(rail_span(entry, row, tick, bg));
        spans.push(blanks(PAD_LEFT as usize, bg));
        if bullet && let Some((glyph, token)) = entry.bullet {
            spans.push(Span::styled(
                format!("{glyph} "),
                on(bullet_style(entry, token, tick), line_bg),
            ));
        }
        let bullet_width = usize::from(entry.bullet.is_some()) * 2;
        let used: usize = content.iter().map(|span| span.content.width()).sum();
        // The band goes *under* the text, not around it. Upstream fills the
        // whole entry area with the background first and paints the content
        // over it (`entry_renderer.rs:715-758`); spans arrive here already
        // styled by whoever built the row, so the background is folded into
        // each of them rather than painted first.
        spans.extend(
            content
                .into_iter()
                .map(|span| Span::styled(span.content, on(span.style, line_bg))),
        );
        // Carry the band to the block's right edge, so a tinted block is a
        // rectangle rather than the shape of its text.
        if line_bg.is_some() {
            let head = if bullet { bullet_width } else { 0 };
            let fill = content_width.saturating_sub(used + head);
            if fill > 0 {
                spans.push(blanks(fill, line_bg));
            }
        }
        if bg.is_some() {
            spans.push(blanks(PAD_RIGHT as usize, bg));
        }
        out.push(Line::from(spans));
    };

    let mut row = 0u16;
    for _ in 0..vpad {
        push(Vec::new(), row, false, false);
        row += 1;
    }
    for (index, content) in entry.rows.iter().enumerate() {
        // The bullet leads the first *content* row, which sits below whatever
        // vertical padding the block asked for.
        push(
            content.line.spans.clone(),
            row,
            content.panel,
            index == 0 && entry.bullet.is_some(),
        );
        row += 1;
    }
    for _ in 0..vpad {
        push(Vec::new(), row, false, false);
        row += 1;
    }
    out
}

/// The accent column at one row.
///
/// Ported from `P/src/scrollback/wrappers/entry_renderer.rs:761-857`, branch
/// for branch and in the same order. The finish flash (400 ms of solid accent
/// on a tool or thinking block that just landed, `:767-789`) is the one branch
/// missing: it keys off a per-entry `finished_at` that Wizard's transcript does
/// not record, and inventing one from the render clock would flash a replayed
/// session's every tool call at startup.
fn rail_span(entry: &Entry, row: u16, tick: u64, bg: Option<Color>) -> Span<'static> {
    let Some(token) = entry.accent else {
        // No accent still *clears* the column with background-styled spaces —
        // it is one cell of chrome that must not show what the row above put
        // there, and under a tinted block it must carry the tint.
        return blanks(ACCENT as usize, bg);
    };
    if entry.pending && entry.animated {
        // Frozen: a solid bar at full colour reads as "paused on you" without
        // the loading-spinner motion.
        return Span::styled(RAIL, on(theme::style(token), bg));
    }
    if entry.animated {
        let brightness = motion::wave(tick, row, WAVE_ROWS, WAVE_SPEED);
        let color = motion::breathe(theme::color(token), brightness);
        return Span::styled(RAIL, on(Style::default().fg(color), bg));
    }
    if entry.mode == Mode::Collapsed && entry.kind.groupable() {
        // A thinner glyph at half brightness, so the rails of a packed run of
        // collapsed entries read as separate blocks rather than one long bar.
        return Span::styled(RAIL_COLLAPSED, on(fade(token, DIM_ACCENT), bg));
    }
    Span::styled(RAIL, on(theme::style(token), bg))
}

/// The `◆` bullet's style.
///
/// Ported from `P/src/scrollback/wrappers/entry_renderer.rs:960-1010`: the
/// bullet waves in step with the rail but is locked to row 0, dims by
/// [`DIM_ACCENT`] when the block is collapsed and groupable, and freezes at
/// full colour while the agent is blocked on the user.
fn bullet_style(entry: &Entry, token: Token, tick: u64) -> Style {
    if entry.pending {
        return theme::style(token);
    }
    if entry.animated {
        let brightness = motion::wave(tick, 0, WAVE_ROWS, WAVE_SPEED);
        return Style::default().fg(motion::breathe(theme::color(token), brightness));
    }
    if entry.mode == Mode::Collapsed && entry.kind.groupable() {
        return fade(token, DIM_ACCENT);
    }
    theme::style(token)
}

/// Content columns inside a block of `width`.
fn content_width(width: u16) -> u16 {
    width.saturating_sub(CHROME_WIDTH).max(1)
}

/// Content columns left once the `◆ ` bullet has taken its two.
///
/// Ported from `P/src/scrollback/types.rs:106-126`: the bullet shrinks
/// `content_width()` and is *not* re-indented under, so a wrapped tool header
/// runs flush left under its own diamond.
fn bulleted_width(width: u16) -> u16 {
    content_width(width).saturating_sub(2).max(1)
}

// ---------------------------------------------------------------------------
// Building entries from Wizard's transcript
// ---------------------------------------------------------------------------

/// Everything a scrollback pass produces: the rows, what each row belongs to,
/// and the image blocks whose rows are waiting for pixels.
struct Scrollback {
    lines: Vec<Line<'static>>,
    tags: Vec<RowTag>,
    blocks: Vec<ImageBlock>,
}

/// Render a conversation into Grok Build's block layout.
///
/// `width` is the block width — the content rect, not the screen.
fn scrollback(
    view: &TranscriptView,
    app: &App,
    cache: &mut ImageCache,
    budget: crate::image_view::ImageBox,
    width: u16,
) -> Scrollback {
    let mut entries: Vec<Entry> = Vec::new();
    let mut blocks: Vec<ImageBlock> = Vec::new();

    for (index, item) in view.iter().enumerate() {
        match item {
            // A turn boundary has no row of its own; the transcript reads as
            // one continuous conversation.
            TranscriptItem::TurnMarker { .. } => {}
            TranscriptItem::User { text, .. } => entries.push(user_entry(text, width)),
            TranscriptItem::Text(message) => entries.push(agent_entry(message, width)),
            TranscriptItem::Thinking(message) => {
                entries.push(thinking_entry(message, width, false))
            }
            TranscriptItem::Tool(tool) => {
                entries.push(tool_entry(tool, app, view.folded(index), index, width))
            }
            TranscriptItem::Notice(message) => entries.push(notice_entry(message, width)),
            TranscriptItem::Images { source, images } => {
                let mut rows: Vec<Row> = Vec::new();
                let mut reserved: Vec<(usize, usize, u16)> = Vec::new();
                for image in images {
                    if let Some(block) = cache.layout(image, budget) {
                        reserved.push((rows.len(), blocks.len(), block.rows));
                        rows.extend(
                            std::iter::repeat_with(|| Row::plain(Line::raw("")))
                                .take(block.rows as usize),
                        );
                        blocks.push(block);
                    }
                    for line in super::image_caption(source, image) {
                        rows.push(Row::plain(line));
                    }
                }
                entries.push(Entry {
                    kind: Kind::Agent,
                    mode: Mode::Expanded,
                    accent: None,
                    animated: false,
                    pending: false,
                    bullet: None,
                    rows,
                    card: None,
                    images: reserved,
                });
            }
        }
    }

    // The uncommitted tail: reasoning and prose arriving right now, decorated
    // exactly like a committed block so nothing shifts sideways when the turn
    // lands.
    let (thinking, streaming) = view.streaming();
    if !thinking.is_empty() {
        entries.push(thinking_entry(thinking, width, true));
    }
    if !streaming.is_empty() {
        let content_width = content_width(width) as usize;
        let mut text = super::render_markdown_at(streaming, content_width);
        let tail = Span::styled("\u{2589}", super::dim());
        match text.lines.last_mut() {
            Some(last) => last.spans.push(tail),
            None => text.lines.push(Line::from(tail)),
        }
        entries.push(Entry {
            kind: Kind::Agent,
            mode: Mode::Expanded,
            accent: None,
            animated: false,
            pending: false,
            bullet: None,
            rows: super::wrap_lines(text, content_width)
                .into_iter()
                .map(Row::plain)
                .collect(),
            card: None,
            images: Vec::new(),
        });
    }

    flatten(&entries, width, app.tick, blocks)
}

/// Decorate every entry and stitch them together with their gap rows, keeping
/// the row tags in lockstep.
fn flatten(entries: &[Entry], width: u16, tick: u64, blocks: Vec<ImageBlock>) -> Scrollback {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut tags: Vec<RowTag> = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        if index > 0 {
            for _ in 0..gap_after(&entries[index - 1], entry) {
                lines.push(Line::raw(""));
            }
        }
        let at = lines.len();
        lines.extend(decorate(entry, width, tick));
        tags.resize(lines.len(), RowTag::Text);
        // The header is the first *content* row, below any vertical padding —
        // clicking a prompt band's margin must not fold anything.
        if let Some(card) = entry.card {
            let header = at + entry.kind.vpad() as usize;
            if header < tags.len() {
                tags[header] = RowTag::Card(card);
            }
        }
        for (offset, slot, rows) in &entry.images {
            for row in 0..*rows {
                let at = at + entry.kind.vpad() as usize + offset + row as usize;
                if at < tags.len() {
                    tags[at] = RowTag::Image { slot: *slot, row };
                }
            }
        }
    }
    Scrollback {
        lines,
        tags,
        blocks,
    }
}

// ---------------------------------------------------------------------------
// User prompts
// ---------------------------------------------------------------------------

/// The user's echoed prompt: `❯ ` then the text, on the one band in the whole
/// transcript.
///
/// Ported from `P/src/scrollback/blocks/user.rs:230-304`. The prefix is
/// `prompt_arrow()` (two columns), continuation rows indent by the same width
/// in the same style, and a recognised `/command` token is restyled to
/// `accent_skill` (`user.rs:44-79`) — Wizard's commands, not Grok's.
///
/// **Divergence:** upstream folds a prompt over three visual lines to three
/// lines plus ` …` (`user.rs:17`, `:500-527`) and offers a key to unfold it.
/// Wizard has no such key, so the prompt is rendered whole: silently hiding
/// what the user typed, with no way to get it back, is not a fold.
fn user_entry(text: &str, width: u16) -> Entry {
    let prefix = theme::style(Token::Accent);
    let body = theme::style(Token::Text);
    let command = theme::style(Token::Heading);
    let inner = content_width(width).saturating_sub(2).max(1) as usize;

    let mut rows: Vec<Row> = Vec::new();
    for (index, source) in text.lines().enumerate() {
        // Slash commands read as commands wherever they appear, which is how
        // upstream marks a skill invocation inside a prompt.
        let spans: Vec<Span<'static>> = match source.split_once(' ') {
            Some((head, rest)) if head.starts_with('/') && index == 0 => vec![
                Span::styled(head.to_string(), command),
                Span::styled(format!(" {rest}"), body),
            ],
            _ if source.starts_with('/') && index == 0 => {
                vec![Span::styled(source.to_string(), command)]
            }
            _ => vec![Span::styled(source.to_string(), body)],
        };
        for wrapped in super::wrap_lines(Text::from(vec![Line::from(spans)]), inner) {
            let head = rows.is_empty();
            let mut spans = vec![Span::styled(if head { PROMPT_ARROW } else { "  " }, prefix)];
            spans.extend(wrapped.spans);
            rows.push(Row::plain(Line::from(spans)));
        }
    }
    if rows.is_empty() {
        rows.push(Row::plain(Line::from(Span::styled(PROMPT_ARROW, prefix))));
    }
    Entry {
        kind: Kind::UserPrompt,
        mode: Mode::Expanded,
        accent: None,
        animated: false,
        pending: false,
        bullet: None,
        rows,
        card: None,
        images: Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Agent messages
// ---------------------------------------------------------------------------

/// An assistant message: markdown, no rail, no background, no bullet.
///
/// Ported from `P/src/scrollback/blocks/agent.rs:208-220`. The accent column is
/// reserved and cleared rather than reclaimed, which is what keeps the prose in
/// the same column as every block that does have a rail.
fn agent_entry(message: &str, width: u16) -> Entry {
    let inner = content_width(width) as usize;
    Entry {
        kind: Kind::Agent,
        mode: Mode::Expanded,
        accent: None,
        animated: false,
        pending: false,
        bullet: None,
        rows: super::wrap_lines(super::render_markdown_at(message, inner), inner)
            .into_iter()
            .map(Row::plain)
            .collect(),
        card: None,
        images: Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Thinking
// ---------------------------------------------------------------------------

/// Model reasoning: a header, a blank row, an `…`, and the last few lines.
///
/// Ported from `P/src/scrollback/blocks/thinking.rs:226-380`. The header is
/// `"Thinking…"` while the reasoning is still arriving and `"Thought"` once it
/// has landed; upstream appends `format!(" for {time_str}")` to the second,
/// which Wizard's transcript has no per-block clock for. The body is faded
/// toward the background (`bg_blend: 0.7`, `R/src/appearance/config.rs:585`);
/// here that is the theme's own faint token plus italics, the house form for
/// reasoning.
///
/// **Divergence:** upstream's `finished_display_mode()` is `Collapsed`
/// (`thinking.rs:510-512`) — reasoning auto-folds to its header once the turn
/// moves on — but its *declared* default is `Truncated` (`:506`), and Wizard
/// has no key to unfold either one. Truncated is what a host with no fold cycle
/// should render: the shape is upstream's and no text becomes unreachable.
fn thinking_entry(message: &str, width: u16, live: bool) -> Entry {
    let inner = bulleted_width(width) as usize;
    let label = if live { "Thinking\u{2026}" } else { "Thought" };
    let body = super::dim().italic();

    let mut rows = vec![
        Row::plain(Line::from(Span::styled(
            label,
            theme::style(Token::Muted).bold(),
        ))),
        Row::plain(Line::raw("")),
    ];
    let wrapped = super::wrap_lines(
        Text::from(
            message
                .lines()
                .map(|line| Line::from(Span::styled(line.to_string(), body)))
                .collect::<Vec<_>>(),
        ),
        inner,
    );
    let hidden = wrapped.len().saturating_sub(THINKING_TAIL);
    if hidden > 0 {
        rows.push(Row::plain(Line::from(Span::styled(
            "\u{2026}",
            super::muted(),
        ))));
    }
    rows.extend(wrapped.into_iter().skip(hidden).map(Row::plain));

    Entry {
        kind: Kind::Thinking,
        mode: Mode::Truncated,
        // `blocks.thinking.accent` defaults to `theme.gray_dim`
        // (`R/src/appearance/config.rs:583`) — the dimmest gray, not the violet
        // `accent_thinking`, which upstream only ever uses for the finish flash.
        accent: Some(Token::Faint),
        animated: live,
        pending: false,
        // Thinking is the block that animates *both* its rail and its bullet:
        // `bullet()` delegates to `accent()` (`thinking.rs:456-462`).
        bullet: Some((DIAMOND, Token::Faint)),
        rows,
        card: None,
        images: Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Notices
// ---------------------------------------------------------------------------

/// A system notice: no rail, no bullet, expanded, groupable.
///
/// Ported from `P/src/scrollback/blocks/system.rs:72-92`.
fn notice_entry(message: &str, width: u16) -> Entry {
    let inner = content_width(width) as usize;
    let style = if message.starts_with("error") {
        theme::style(Token::Error).bold()
    } else {
        super::dim().italic()
    };
    Entry {
        kind: Kind::System,
        mode: Mode::Expanded,
        accent: None,
        animated: false,
        pending: false,
        bullet: None,
        rows: super::wrap_all(
            message
                .lines()
                .map(|line| Line::from(Span::styled(line.to_string(), style)))
                .collect(),
            inner,
        )
        .into_iter()
        .map(Row::plain)
        .collect(),
        card: None,
        images: Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

/// Which of upstream's tool blocks a Wizard tool call reads as.
///
/// Every variant owns its own header upstream — `P/src/scrollback/blocks/tool/`
/// is a sum type that delegates and no more (`mod.rs:190-297`) — so this is a
/// mapping of Wizard's tool names onto those headers, not onto a generic one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolKind {
    /// `read_file` → `Read <path> (1-120)`.
    Read,
    /// `write_file` / `edit_file` → `Creating <path>` / `Edit <path>`.
    Edit,
    /// `execute` → `Run <command>`. The only variant with a *success* rail.
    Execute,
    /// `search_files` → `Search "pattern" in <path>`.
    Search,
    /// `list_files` → `List <path> (14 entries)`.
    ListDir,
    /// `web_fetch` → `Fetch <url>`.
    Fetch,
    /// `web_search` / `x_search` → `Web Search <query>`.
    WebSearch,
    /// Everything else, including MCP tools.
    Other,
}

impl ToolKind {
    fn of(name: &str) -> ToolKind {
        match name {
            "read_file" => ToolKind::Read,
            "write_file" | "edit_file" => ToolKind::Edit,
            "execute" => ToolKind::Execute,
            "search_files" => ToolKind::Search,
            "list_files" => ToolKind::ListDir,
            "web_fetch" => ToolKind::Fetch,
            "web_search" | "x_search" => ToolKind::WebSearch,
            _ => ToolKind::Other,
        }
    }

    /// The rail this variant paints once it has finished cleanly.
    ///
    /// Ported from the `accent()` implementations: Read, Search and ListDir
    /// return `None` *ever* (`read.rs:406`, `search.rs:522`, `list_dir.rs:198`)
    /// — they are cheap and quiet and get no bar at all; Execute finishes in
    /// `accent_success` (`execute.rs:695-709`); everything else finishes in
    /// `accent_tool`, the mid gray (`other.rs:310-323`).
    fn done_accent(self) -> Option<Token> {
        match self {
            ToolKind::Read | ToolKind::Search | ToolKind::ListDir | ToolKind::Edit => None,
            ToolKind::Execute => Some(Token::Success),
            _ => Some(Token::Muted),
        }
    }
}

/// A tool call as a Grok Build block.
///
/// `spawn_subagent` and a backgrounded `execute` are routed to the subagent and
/// bg-task rows instead: upstream has real blocks for both, and mapping
/// Wizard's onto them is the whole point of wearing this chrome.
fn tool_entry(tool: &ToolItem, app: &App, folded: bool, index: usize, width: u16) -> Entry {
    if tool.name == "spawn_subagent" {
        return subagent_entry(tool, app, index, width);
    }
    if tool.name == "execute"
        && tool.args.get("run_in_background") == Some(&serde_json::json!(true))
    {
        return bg_task_entry(tool, index, width);
    }

    let kind = ToolKind::of(&tool.name);
    let running = tool.output.is_none();
    let failed = tool.output.as_ref().is_some_and(|out| out.is_error);
    let mode = if folded {
        Mode::Collapsed
    } else if kind == ToolKind::Execute || kind == ToolKind::Read {
        Mode::Truncated
    } else {
        Mode::Expanded
    };

    let mut rows = vec![Row::plain(super::truncate_line(
        Line::from(tool_header(tool, kind, mode)),
        bulleted_width(width) as usize,
    ))];
    if mode != Mode::Collapsed {
        rows.extend(tool_output(tool, kind, running, width));
    }

    Entry {
        kind: Kind::Tool,
        mode,
        accent: tool_accent(kind, running, failed, mode),
        animated: running,
        pending: false,
        bullet: Some(tool_bullet(running, failed)),
        rows,
        card: Some(index),
        images: Vec::new(),
    }
}

/// The rail a tool call paints.
///
/// Ported from `P/src/scrollback/blocks/tool/other.rs:310-323` and
/// `tool/execute.rs:695-709`: a *collapsed* call of the "other" shape paints no
/// rail at all, a failure is `accent_error`, a run in flight is an animated
/// `accent_running`, and a finish is per-variant (see
/// [`ToolKind::done_accent`]).
fn tool_accent(kind: ToolKind, running: bool, failed: bool, mode: Mode) -> Option<Token> {
    if failed {
        return Some(Token::ToolFailed);
    }
    if running {
        return Some(Token::ToolRunning);
    }
    if mode == Mode::Collapsed && kind == ToolKind::Other {
        return None;
    }
    kind.done_accent()
}

/// The bullet leading a tool header.
///
/// **Divergence, and a deliberate one.** Upstream's bullet is always `◆` and
/// its status is the colour alone (`R/src/appearance/config.rs:620`,
/// `ToolBullet::Diamond`; the check and ballot glyphs appear only on per-hook
/// detail rows). Wizard's first design rule is that meaning never rests on hue,
/// because the TUI has to stay readable at 16 colours and under `NO_COLOR`, so
/// a failed call swaps the diamond for `✗`. Only failure: success and progress
/// are legible from context, and losing "this one broke" is the single state
/// where a monochrome reader would actually be misled. The glyphs come from the
/// skin table so this and the house frame cannot drift apart.
fn tool_bullet(running: bool, failed: bool) -> (&'static str, Token) {
    let chrome = crate::skin::chrome();
    match (running, failed) {
        (_, true) => (chrome.tool_failed, Token::ToolFailed),
        (true, _) => (DIAMOND, Token::ToolRunning),
        _ => (chrome.tool_done, Token::Muted),
    }
}

/// A tool header: a bold verb, a coloured operand, a dim detail suffix.
///
/// Ported from the per-variant header builders in
/// `P/src/scrollback/blocks/tool/` (the table at §4.1 of the port notes:
/// `read.rs:184`, `edit.rs:846-865`, `execute.rs:212`, `search.rs:259`,
/// `list_dir.rs:114`, `web_fetch.rs:117`, `web_search.rs:115`,
/// `other.rs:153-169`). A collapsed header is muted throughout
/// (`ToolConfig { muted_collapsed: true, dim_details: true }`,
/// `R/src/appearance/config.rs:617-621`).
///
/// Upstream paints paths in `theme.path` (orange) and commands in
/// `theme.command` (yellow). Wizard has no token for either, and inventing one
/// would put two more hues in the palette contract for one skin; both are
/// literal code, so both ask for [`Token::Code`].
fn tool_header(tool: &ToolItem, kind: ToolKind, mode: Mode) -> Vec<Span<'static>> {
    let collapsed = mode == Mode::Collapsed;
    let verb = if collapsed {
        theme::style(Token::Muted)
    } else {
        theme::style(Token::Text).bold()
    };
    let operand = if collapsed {
        theme::style(Token::Muted)
    } else {
        theme::style(Token::Code)
    };
    let detail = super::dim();
    let arg = |key: &str| {
        tool.args
            .get(key)
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string()
    };
    let output = tool.output.as_ref().map(|out| out.content.as_str());

    let mut spans = Vec::new();
    match kind {
        ToolKind::Read => {
            spans.push(Span::styled("Read ", verb));
            spans.push(Span::styled(arg("path"), operand));
            let range = match (tool.args.get("start_line"), tool.args.get("end_line")) {
                (Some(start), Some(end)) => format!(" ({start}-{end})"),
                (Some(start), None) => format!(" ({start}-)"),
                _ => String::new(),
            };
            spans.push(Span::styled(range, detail));
        }
        ToolKind::Edit => {
            spans.push(Span::styled(
                if tool.name == "write_file" {
                    "Creating "
                } else {
                    "Edit "
                },
                verb,
            ));
            spans.push(Span::styled(arg("path"), operand));
        }
        ToolKind::Execute => {
            spans.push(Span::styled("Run ", verb));
            spans.push(Span::styled(
                super::truncate_width(arg("command").trim(), SUMMARY_WIDTH),
                operand,
            ));
        }
        ToolKind::Search => {
            spans.push(Span::styled("Search ", verb));
            // Rust-debug-quoted, exactly as upstream renders it
            // (`search.rs:246`): a pattern with a space or a quote in it has to
            // read as one token.
            spans.push(Span::styled(format!("{:?}", arg("pattern")), operand));
            let path = arg("path");
            if !path.is_empty() {
                spans.push(Span::styled(" in ", detail));
                spans.push(Span::styled(path, operand));
            }
            if let Some(text) = output {
                spans.push(Span::styled(format!(" {}", match_summary(text)), detail));
            }
        }
        ToolKind::ListDir => {
            spans.push(Span::styled("List ", verb));
            let path = arg("path");
            spans.push(Span::styled(
                if path.is_empty() {
                    ".".to_string()
                } else {
                    path
                },
                operand,
            ));
            if let Some(text) = output {
                let n = text.lines().filter(|line| !line.trim().is_empty()).count();
                spans.push(Span::styled(
                    format!(" ({n} {})", if n == 1 { "entry" } else { "entries" }),
                    detail,
                ));
            }
        }
        ToolKind::Fetch => {
            spans.push(Span::styled("Fetch ", verb));
            spans.push(Span::styled(
                super::truncate_width(&arg("url"), SUMMARY_WIDTH),
                operand,
            ));
        }
        ToolKind::WebSearch => {
            spans.push(Span::styled("Web Search ", verb));
            spans.push(Span::styled(
                super::truncate_width(&arg("query"), SUMMARY_WIDTH),
                operand,
            ));
        }
        ToolKind::Other => {
            spans.push(Span::styled(tool.name.clone(), verb));
            let summary = other_summary(tool);
            if !summary.is_empty() {
                // Two spaces, as upstream (`other.rs:169`).
                spans.push(Span::styled(
                    format!("  {}", super::truncate_width(&summary, SUMMARY_WIDTH)),
                    detail,
                ));
            }
        }
    }
    spans
}

/// The one obvious argument of a tool that has one, else its JSON.
///
/// The same reading `crate::ui::tool_label` takes: `{"command":"ls -la"}` says
/// nothing `ls -la` does not and costs three quarters of the width to say it.
fn other_summary(tool: &ToolItem) -> String {
    if tool.args.is_null() {
        return String::new();
    }
    match tool
        .args
        .get("command")
        .or_else(|| tool.args.get("path"))
        .or_else(|| tool.args.get("query"))
    {
        Some(serde_json::Value::String(subject)) => subject.clone(),
        _ => serde_json::to_string(&tool.args).unwrap_or_default(),
    }
}

/// `(3 matches in 2 files)` and its neighbours.
///
/// Ported from `P/src/scrollback/blocks/tool/search.rs:177-213`. Wizard's
/// `search_files` returns `path:line:text` rows, so the counts are read back
/// off the output rather than carried alongside it.
fn match_summary(output: &str) -> String {
    let rows: Vec<&str> = output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    if rows.is_empty() {
        return "(no matches)".to_string();
    }
    let mut files: Vec<&str> = rows
        .iter()
        .filter_map(|line| line.split_once(':').map(|(path, _)| path))
        .collect();
    files.sort_unstable();
    files.dedup();
    match (rows.len(), files.len()) {
        (1, _) => "(1 match)".to_string(),
        (n, 0 | 1) => format!("({n} matches)"),
        (n, m) => format!("({n} matches in {m} files)"),
    }
}

/// A tool's output preview.
///
/// Ported from the per-variant renderers (§4.3 of the port notes): a command's
/// output is flush left on a `bg_dark` panel band with a two-sided window
/// (`execute.rs:567-590`), a file read gets a right-aligned line-number gutter
/// and a bare `…` with no count (`read.rs:275-313`), and everything else is
/// indented two columns and capped (`use_tool.rs:196-204`).
///
/// Upstream's "press Enter to view" names a key Wizard does not bind; the hint
/// here names the one that works, because a hint that names the wrong key is
/// worse than none.
fn tool_output(tool: &ToolItem, kind: ToolKind, running: bool, width: u16) -> Vec<Row> {
    let text = match tool.output.as_ref() {
        Some(out) => out.content.as_str(),
        None if !tool.progress.is_empty() => tool.progress.as_str(),
        None => return Vec::new(),
    };
    if text.trim().is_empty() {
        return match kind {
            ToolKind::Search => vec![Row::plain(indented("(no results)", super::dim()))],
            ToolKind::Fetch | ToolKind::WebSearch => {
                vec![Row::plain(indented("(no content)", super::dim()))]
            }
            _ => Vec::new(),
        };
    }
    let body = super::muted();
    let marker = super::dim();
    let lines: Vec<&str> = text.lines().collect();

    match kind {
        ToolKind::Execute => {
            // Flush left inside the block, on a panel band, wrapped two columns
            // short of the content width (`execute.rs:567`).
            let inner = content_width(width).saturating_sub(2).max(20) as usize;
            let mut rows: Vec<Row> = Vec::new();
            let emit = |slice: &[&str], rows: &mut Vec<Row>| {
                for line in slice {
                    for wrapped in super::wrap_lines(
                        Text::from(vec![Line::from(Span::styled((*line).to_string(), body))]),
                        inner,
                    ) {
                        rows.push(Row::panel(wrapped));
                    }
                }
            };
            if lines.len() > EXECUTE_FIRST + EXECUTE_LAST && !running {
                let hidden = lines.len() - EXECUTE_FIRST - EXECUTE_LAST;
                emit(&lines[..EXECUTE_FIRST], &mut rows);
                rows.push(Row::panel(Line::from(Span::styled(
                    format!("\u{2026} +{hidden} lines"),
                    marker,
                ))));
                emit(&lines[lines.len() - EXECUTE_LAST..], &mut rows);
            } else {
                // A command still running is read from its tail: the line it is
                // waiting on is the last one.
                let shown = lines.len().min(MAX_INLINE);
                if lines.len() > shown {
                    rows.push(Row::panel(Line::from(Span::styled(
                        format!("\u{2026} +{} earlier lines", lines.len() - shown),
                        marker,
                    ))));
                }
                emit(&lines[lines.len() - shown..], &mut rows);
            }
            rows
        }
        ToolKind::Read => {
            // A right-aligned line-number gutter with two trailing spaces
            // (`read.rs:275`), and a bare `…` with no count for the elision
            // (`read.rs:313`) — the file's own numbering says how much is gone.
            let gutter = lines.len().to_string().len();
            let mut rows: Vec<Row> = Vec::new();
            let emit = |range: std::ops::Range<usize>, rows: &mut Vec<Row>| {
                for (offset, line) in lines[range.clone()].iter().enumerate() {
                    let number = range.start + offset + 1;
                    rows.push(Row::plain(Line::from(vec![
                        Span::styled(format!("{number:>gutter$}  "), marker),
                        Span::styled((*line).to_string(), body),
                    ])));
                }
            };
            if lines.len() > READ_FIRST + READ_LAST {
                emit(0..READ_FIRST, &mut rows);
                rows.push(Row::plain(Line::from(Span::styled("\u{2026}", marker))));
                emit(lines.len() - READ_LAST..lines.len(), &mut rows);
            } else {
                emit(0..lines.len(), &mut rows);
            }
            rows
        }
        _ => {
            let shown = if lines.len() > MAX_INLINE {
                TRUNCATED_INLINE
            } else {
                lines.len()
            };
            let mut rows: Vec<Row> = lines[..shown]
                .iter()
                .map(|line| Row::plain(indented(line, body)))
                .collect();
            if lines.len() > shown {
                rows.push(Row::plain(indented(
                    &format!("... ({} more lines, ctrl+t to expand)", lines.len() - shown),
                    marker,
                )));
            }
            rows
        }
    }
}

/// A two-column indented output row (`use_tool.rs`, `search.rs:447`).
fn indented(text: &str, style: Style) -> Line<'static> {
    Line::from(vec![Span::raw("  "), Span::styled(text.to_string(), style)])
}

// ---------------------------------------------------------------------------
// Subagent and background-task rows
// ---------------------------------------------------------------------------

/// A delegated subagent, permanently one row.
///
/// Ported from `P/src/scrollback/blocks/subagent.rs:189-308`: a bold
/// `"Subagent "` label, a state verb, the description in curly quotes, the
/// persona in parentheses, and an activity suffix after an em dash. The rail is
/// **static** `accent_running` while the run is going and only the *bullet*
/// animates (`subagent.rs:263-281`) — the one block that splits them.
///
/// Wizard's own state comes from the matching [`SubagentPane`], which is what
/// keeps a background subagent visible in the transcript as well as on the rail
/// and the status bar.
fn subagent_entry(tool: &ToolItem, app: &App, index: usize, width: u16) -> Entry {
    let who = tool
        .args
        .get("subagent")
        .and_then(|value| value.as_str())
        .unwrap_or("subagent");
    let task = tool
        .args
        .get("task")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    let background = tool.args.get("background") == Some(&serde_json::json!(true));
    let pane = app
        .panes
        .iter()
        .find(|pane| pane.name == who && pane.task == task);
    let running = tool.output.is_none();
    let failed = tool.output.as_ref().is_some_and(|out| out.is_error);

    let state = match (pane, running, failed) {
        (_, false, true) => format!("failed in {}: ", elapsed_of(pane)),
        (_, false, false) => format!("completed in {}: ", elapsed_of(pane)),
        (_, true, _) if background => "started: ".to_string(),
        _ => "running: ".to_string(),
    };
    let mut spans = vec![
        Span::styled("Subagent ", theme::style(Token::Text).bold()),
        Span::styled(state, super::muted()),
        Span::styled(
            format!(
                "\u{201c}{}\u{201d}",
                super::truncate_width(task, SUMMARY_WIDTH)
            ),
            theme::style(Token::Code),
        ),
        Span::styled(format!(" ({who})"), super::dim()),
    ];
    if let Some(pane) = pane.filter(|pane| pane.status == PaneStatus::Running) {
        let activity = live_activity(pane);
        if !activity.is_empty() {
            spans.push(Span::styled(
                format!(" \u{2014} {}", super::truncate_width(&activity, 24)),
                super::dim(),
            ));
        }
    }

    Entry {
        kind: Kind::Subagent,
        mode: Mode::Collapsed,
        accent: running.then_some(Token::ToolRunning).or(if failed {
            Some(Token::ToolFailed)
        } else {
            Some(Token::Muted)
        }),
        // Static rail, animated bullet: the split upstream is easy to miss.
        animated: false,
        pending: false,
        bullet: Some(if failed {
            (crate::skin::chrome().tool_failed, Token::ToolFailed)
        } else if running {
            (DIAMOND, Token::ToolRunning)
        } else {
            (DIAMOND, Token::Muted)
        }),
        rows: vec![Row::plain(super::truncate_line(
            Line::from(spans),
            bulleted_width(width) as usize,
        ))],
        card: Some(index),
        images: Vec::new(),
    }
}

/// A detached background command, permanently one row.
///
/// Ported from `P/src/scrollback/blocks/bg_task.rs:150-252`: `"Task "` then
/// `started:` / `completed in {t}:` / `{verb} in {t}:`, where the verb is
/// `"killed"` for a signal and `"failed"` otherwise.
fn bg_task_entry(tool: &ToolItem, index: usize, width: u16) -> Entry {
    let command = tool
        .args
        .get("command")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    let running = tool.output.is_none();
    let failed = tool.output.as_ref().is_some_and(|out| out.is_error);
    let killed = tool.output.as_ref().is_some_and(|out| {
        let text = out.content.to_ascii_lowercase();
        ["killed", "sigterm", "sigkill", "oom"]
            .iter()
            .any(|needle| text.contains(needle))
    });
    let state = match (running, failed, killed) {
        (true, ..) => "started: ",
        (_, true, true) => "killed: ",
        (_, true, false) => "failed: ",
        _ => "completed: ",
    };

    Entry {
        kind: Kind::BgTask,
        mode: Mode::Collapsed,
        accent: Some(if failed {
            Token::ToolFailed
        } else if running {
            Token::ToolRunning
        } else {
            Token::Muted
        }),
        animated: false,
        pending: false,
        bullet: Some(if failed {
            (crate::skin::chrome().tool_failed, Token::ToolFailed)
        } else if running {
            (DIAMOND, Token::ToolRunning)
        } else {
            (DIAMOND, Token::Muted)
        }),
        rows: vec![Row::plain(super::truncate_line(
            Line::from(vec![
                Span::styled("Task ", theme::style(Token::Text).bold()),
                Span::styled(state, super::muted()),
                Span::styled(
                    format!(
                        "\u{201c}{}\u{201d}",
                        super::truncate_width(command.trim(), SUMMARY_WIDTH)
                    ),
                    theme::style(Token::Code),
                ),
            ]),
            bulleted_width(width) as usize,
        ))],
        card: Some(index),
        images: Vec::new(),
    }
}

/// What a subagent is doing *right now*, as a suffix distinct from its task.
///
/// Upstream carries a separate `activity_label` alongside the description
/// (`P/src/views/tasks_pane.rs:403-410`), so the em-dash suffix is always new
/// information. Wizard's [`SubagentPane::activity`] falls back to the task when
/// there is nothing more specific, which would print the description twice — so
/// a fallback that repeats the task is treated as no activity at all.
fn live_activity(pane: &SubagentPane) -> String {
    let activity = pane.activity().trim().lines().next().unwrap_or("").trim();
    if activity.is_empty() || activity == pane.task.trim() {
        return String::new();
    }
    activity.to_string()
}

/// A pane's run time, formatted upstream's way.
fn elapsed_of(pane: Option<&SubagentPane>) -> String {
    pane.map_or_else(|| "?".to_string(), |pane| format_duration(pane.elapsed()))
}

// ---------------------------------------------------------------------------
// Duration and token formatting
// ---------------------------------------------------------------------------

/// `0.2s` / `42s` / `1m20s` / `2h13m`.
///
/// Ported from `R/src/util.rs:85-101` (xai-org/grok-build, Apache-2.0).
fn format_duration(duration: Duration) -> String {
    let secs = duration.as_secs_f64();
    if secs < 10.0 {
        format!("{secs:.1}s")
    } else if secs < 60.0 {
        format!("{}s", secs as u64)
    } else if secs < 3600.0 {
        let total = secs as u64;
        format!("{}m{}s", total / 60, total % 60)
    } else {
        let total = secs as u64;
        format!("{}h{}m", total / 3600, (total % 3600) / 60)
    }
}

/// `999` / `1.23k` / `10.1k` / `500k` / `1.23m`.
///
/// Ported from `P/src/views/turn_status.rs:846-866`.
fn format_tokens_short(tokens: u64) -> String {
    match tokens {
        0..=999 => tokens.to_string(),
        1_000..=9_999 => format!("{:.2}k", tokens as f64 / 1_000.0),
        10_000..=99_999 => format!("{:.1}k", tokens as f64 / 1_000.0),
        100_000..=999_999 => format!("{}k", tokens / 1_000),
        _ => format!("{:.2}m", tokens as f64 / 1_000_000.0),
    }
}

// ---------------------------------------------------------------------------
// Screen layout
// ---------------------------------------------------------------------------

/// The rows this skin lays a frame out into, top to bottom.
///
/// Grok Build's agent view (`P/src/views/agent.rs:196-236`): an outer margin,
/// a right-aligned status bar, the scrollback, the turn status on its own row
/// with a gap either side, the composer, and a shortcuts bar. Wizard adds two
/// bands of its own — the todo list and the subagent rail — because both carry
/// state that has nowhere else to go, and a skin is not allowed to hide state.
#[derive(Debug, Clone, Copy)]
struct Screen {
    status: Rect,
    body: Rect,
    todo: Rect,
    turn: Rect,
    composer: Rect,
    /// Grok Build's tasks pane: background commands and delegated subagents.
    tasks: Rect,
    shortcuts: Rect,
}

/// Lay `area` out. `turn_rows` is 1 when the turn status has something to say.
fn screen(app: &App, area: Rect, turn_rows: u16, composer_rows: u16) -> Screen {
    let compact = area.height < COMPACT_HEIGHT;
    let vpad = if compact { 0 } else { OUTER_VPAD };
    let gap = u16::from(!compact);
    let content = Rect {
        x: area.x + OUTER_HPAD,
        width: area.width.saturating_sub(OUTER_HPAD * 2),
        ..area
    };

    // Bottom-up, because everything below the scrollback has a fixed height
    // and the scrollback takes what is left. `take` never underflows, so a
    // terminal too short for a band simply does not get it.
    let mut floor = area.bottom().saturating_sub(vpad);
    let mut take = |rows: u16| -> Rect {
        let rows = rows.min(floor.saturating_sub(area.y));
        floor -= rows;
        Rect {
            y: floor,
            height: rows,
            ..content
        }
    };
    let shortcuts = take(1);
    take(gap);
    let tasks = take(tasks_height(app));
    let composer = take(composer_rows);
    take(turn_rows.min(gap));
    let turn = take(turn_rows);
    take(turn_rows.min(gap));
    let todo = take(todos_height(
        app,
        area,
        composer_rows + turn_rows + tasks.height + 1 + vpad,
    ));

    let top = area.y + vpad;
    let status = Rect {
        y: top,
        height: 1u16.min(floor.saturating_sub(top)),
        ..content
    };
    let body_top = (status.bottom() + gap).min(floor);
    Screen {
        status,
        body: Rect {
            y: body_top,
            height: floor.saturating_sub(body_top),
            ..content
        },
        todo,
        turn,
        composer,
        tasks,
        shortcuts,
    }
}

/// Rows the composer occupies: its draft, clamped, plus its two border rows.
fn composer_rows(app: &App, width: u16) -> u16 {
    let budget = composer_budget(width);
    (super::wrap_rows(&super::composer_chars(app), budget).len() as u16)
        .clamp(1, super::MAX_INPUT_ROWS)
        + 2
}

/// Columns of draft the composer holds.
///
/// The box is painted over the outermost column on each side and the content
/// is inset by `chrome_pad_left`/`chrome_pad_right` (both 2, `render.rs:983`),
/// of which the `❯ ` prefix takes two more
/// (`P/src/views/prompt_widget/mod.rs:2927-3028`).
fn composer_budget(width: u16) -> usize {
    (width as usize)
        .saturating_sub(PAD_LEFT as usize + PAD_RIGHT as usize + 2)
        .max(1)
}

/// Render one frame in Grok Build's chrome.
pub(super) fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let status = turn_status(app);
    let content_width = area.width.saturating_sub(OUTER_HPAD * 2);
    let rows = composer_rows(app, content_width);
    let layout = screen(app, area, u16::from(status.is_some()), rows);

    if let Some(pane) = app.attached_pane() {
        super::draw_pane(frame, app, pane, layout.body);
    } else if app.diff.is_some() {
        let [chat, side] =
            Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)])
                .areas(layout.body);
        draw_scrollback(frame, app, chat);
        super::draw_diff_sidebar(frame, app, side);
    } else {
        draw_scrollback(frame, app, layout.body);
    }

    draw_status_bar(frame, app, layout.status);
    if layout.todo.height > 0 {
        draw_todos(frame, app, layout.todo);
    }
    if let Some(status) = status
        && layout.turn.height > 0
    {
        frame.render_widget(
            Paragraph::new(status.line(layout.turn.width as usize)),
            layout.turn,
        );
    }
    draw_composer(frame, app, layout.composer);
    if layout.tasks.height > 0 {
        draw_tasks_pane(frame, app, layout.tasks);
    }
    draw_shortcuts(frame, app, layout.shortcuts);

    if !super::overlay_open(app) {
        super::draw_suggestions(frame, app, layout.composer);
    }
    if app.picker.is_some() {
        super::draw_picker(frame, app);
    }
    if app.plan_review.is_some() {
        super::draw_plan_review(frame, app);
    }
    if app.interview.is_some() {
        super::draw_interview(frame, app);
    }
    if app.show_dashboard {
        super::draw_dashboard(frame, app);
    }
    if super::overlay_open(app) {
        app.card_hits.borrow_mut().clear();
    }

    // The selection highlight paints last so it reverses whatever ended up on
    // screen, overlay included.
    if let Some(selection) = app.selection {
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
// The scrollback pane
// ---------------------------------------------------------------------------

/// The transcript, and the scrollbar in the far-right column.
fn draw_scrollback(frame: &mut Frame, app: &App, area: Rect) {
    app.card_hits.borrow_mut().clear();
    if app.welcome_visible() {
        if !super::overlay_open(app) {
            draw_welcome(frame, app, area);
        }
        return;
    }
    if area.width == 0 || area.height == 0 {
        return;
    }

    let mut cache = app.images.borrow_mut();
    // Images hang in the content column, so the box handed to the cache is the
    // block's content area rather than the whole pane.
    let inner = area.inner(Margin {
        horizontal: 0,
        vertical: 0,
    });
    let Scrollback {
        lines,
        tags,
        blocks,
    } = scrollback(
        &app.transcript,
        app,
        &mut cache,
        super::image_box(Rect {
            width: content_width(area.width),
            ..inner
        }),
        area.width,
    );

    let height = area.height as usize;
    let total = lines.len();
    let max_scroll = total.saturating_sub(height);
    app.transcript.max_scroll.set(max_scroll as u16);
    let start = if app.transcript.follow || max_scroll == 0 {
        max_scroll
    } else {
        (app.transcript.scroll as usize).min(max_scroll)
    };
    let remaining = max_scroll.saturating_sub(start);
    let end = (start + height).min(total);
    let visible: Vec<Line<'static>> = lines[start..end].to_vec();

    {
        let mut hits = app.card_hits.borrow_mut();
        for (offset, tag) in tags[start..end].iter().enumerate() {
            if let RowTag::Card(index) = tag {
                hits.push((area.y + offset as u16, *index));
            }
        }
    }

    let first_row_width = visible.first().map(|line| line.width() as u16).unwrap_or(0);
    frame.render_widget(Paragraph::new(Text::from(visible)), area);

    if !super::overlay_open(app) {
        // Shift the origin so `IMAGE_INDENT` lands on the block's content
        // column: one for the rail, two for the left pad.
        let origin = Rect {
            x: area.x + ACCENT + PAD_LEFT - super::IMAGE_INDENT,
            ..area
        };
        super::paint_images(frame, origin, &tags[start..end], &blocks, &mut cache);
    }

    if remaining > 0 {
        let label = format!("\u{2193} {remaining} more ");
        let width = (label.width() as u16).min(area.width);
        if first_row_width + width <= area.width {
            let hint = Rect {
                x: area.right().saturating_sub(width),
                y: area.y,
                width,
                height: 1,
            };
            frame.render_widget(Clear, hint);
            frame.render_widget(Paragraph::new(Span::styled(label, super::dim())), hint);
        }
    }

    // The scrollbar sits in the very last screen column, outside the block
    // layout entirely (`P/src/views/agent.rs:363`, `gap_left`/`gap_right` both
    // 0). Upstream paints a `scrollbar_bg` track under a `scrollbar_fg` thumb;
    // the house rule is that nothing paints a background, so both are drawn as
    // foreground glyphs instead — the two-tone bar survives, the opaque slab
    // does not.
    if total > height {
        let mut state = ScrollbarState::new(max_scroll + 1).position(start);
        let bar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .track_symbol(Some("\u{2591}"))
            .track_style(super::dim())
            .thumb_symbol("\u{2588}")
            .thumb_style(super::muted());
        frame.render_stateful_widget(
            bar,
            Rect {
                x: area.x,
                y: area.y,
                width: area.width + OUTER_HPAD,
                height: area.height,
            },
            &mut state,
        );
    }
}

// ---------------------------------------------------------------------------
// The turn status row
// ---------------------------------------------------------------------------

/// The turn status: `⠧ Run cargo test 12.4s        1m20s ⇣12k [stop]`.
///
/// Ported from `P/src/views/turn_status.rs`. The wording is upstream's fixed
/// set — there is no shuffled verb pool anywhere in Grok Build, which is why
/// Wizard's `spinner_verb` does not appear here — mapped onto the states Wizard
/// actually has. Returns `None` when there is nothing to narrate, and the row
/// then costs no screen space.
fn turn_status(app: &App) -> Option<TurnStatus> {
    let gray = super::dim();

    // Blocked on the user: the braille spinner is replaced by a pulsing `◆` and
    // the accent rails freeze (`turn_status.rs:280-294`, `:60-63`). Every
    // "your turn" cue in the UI is this same diamond at this same cadence.
    if let Some(label) = waiting_on_user(app) {
        let brightness = motion::pulse(app.tick, PULSE_SPEED);
        // Never all the way out: 0.3 at the trough, so the diamond stays
        // visible (`turn_status.rs:61-62`).
        let color = motion::breathe(theme::color(Token::Accent), 0.3 + brightness * 0.7);
        return Some(TurnStatus {
            left: vec![
                Span::styled(format!("{DIAMOND} "), Style::default().fg(color)),
                Span::styled(label, gray),
            ],
            // Parked never falls through to the running-turn chrome: the wait
            // ends the moment the user acts, so a running clock and a `[stop]`
            // would both be lying.
            right: String::new(),
        });
    }

    if !app.status.busy && app.rebuilding.is_none() && !app.compacting {
        // Idle, but something is still running in the background: upstream's
        // `format_still_running` cue, on the calmer monitor cadence.
        let still = still_running(app)?;
        let frame = MONITOR[(app.tick / MONITOR_DIVISOR) as usize % MONITOR.len()];
        return Some(TurnStatus {
            left: vec![
                Span::styled(format!("{frame} "), theme::style(Token::Heading)),
                Span::styled(still, gray),
            ],
            right: String::new(),
        });
    }

    let spinner = SPINNER[(app.tick / SPINNER_DIVISOR) as usize % SPINNER.len()];
    let (label, style) = activity(app);
    let mut left = vec![
        Span::styled(format!("{spinner} "), style),
        Span::styled(label, style),
    ];
    // The step counter, in the slot and the gray upstream gives its *phase*
    // timer (`turn_status.rs:433-448`) — the field that says how far into the
    // current activity the turn is. Grok Build has no step budget and so no
    // counter, but Wizard does, and a skin restyles what is on screen rather
    // than withholding from it. The capped form shows its denominator; the
    // default unlimited budget has none to show.
    left.push(Span::styled(
        match app.status.max_steps.cap() {
            Some(cap) => format!(" step {}/{cap}", app.status.step),
            None => format!(" step {}", app.status.step),
        },
        gray,
    ));
    // Queued prompts. Upstream's parked cue is `· N queued — Enter to send
    // now`; Wizard's Enter queues rather than sending, so the copy says what
    // this Enter does.
    if !app.message_queue.is_empty() {
        left.push(Span::styled(
            format!(" \u{00b7} {} queued", app.message_queue.len()),
            gray,
        ));
    }

    // Right-hand side: the turn timer, the token count, and `[stop]`.
    //
    // Upstream also carries a *phase* timer on the left — how long this one
    // activity has been going, separately from the turn. Wizard clocks only the
    // turn (`App::turn_started`), and printing the same number in both places
    // is all that would come of carrying both, so the one clock goes where
    // upstream's turn timer is.
    let tokens = app.status.prompt_tokens + app.status.completion_tokens;
    let mut right = match app.turn_started {
        Some(started) => format_duration(started.elapsed()),
        None => String::new(),
    };
    if tokens > 0 {
        right.push_str(&format!(" {TOKEN_ARROW}{}", format_tokens_short(tokens)));
    }
    // `[stop]` is upstream's cancel affordance and there is deliberately no
    // "esc to interrupt" string anywhere in Grok Build. Wizard's hit map lives
    // in `App` and this renderer must not add to it, so the button is a label
    // rather than a target — the shortcuts bar directly below names the key
    // that does it.
    right.push_str(" [stop]");
    Some(TurnStatus { left, right })
}

/// The turn status as two halves, because the row is built by measuring the
/// right-hand content first and letting the activity label truncate into
/// whatever is left (`P/src/views/turn_status.rs:371-412`).
struct TurnStatus {
    left: Vec<Span<'static>>,
    right: String,
}

impl TurnStatus {
    /// Lay the two halves out across `width`.
    fn line(self, width: usize) -> Line<'static> {
        let right_width = self.right.width();
        let mut left = super::truncate_line(
            Line::from(self.left),
            width.saturating_sub(right_width + 1).max(1),
        );
        if right_width > 0 {
            let gap = width.saturating_sub(left.width() + right_width);
            left.spans.push(Span::raw(" ".repeat(gap)));
            left.spans.push(Span::styled(self.right, super::dim()));
        }
        left
    }
}

/// What the agent is doing, and the colour it reads in.
///
/// Ported from `compute_activity` (`P/src/views/turn_status.rs:656-762`) and
/// the tool rows at `:499-557`. Every string is upstream's; which one applies
/// is read off Wizard's state.
fn activity(app: &App) -> (String, Style) {
    let secondary = theme::style(Token::Muted);
    if let Some(label) = &app.rebuilding {
        return (format!("{label}\u{2026}"), secondary);
    }
    if app.compacting {
        return ("Compacting\u{2026}".to_string(), secondary);
    }
    // A tool in flight names itself: a gray verb and a coloured operand, the
    // same grammar the scrollback header uses.
    if let Some(tool) = running_tool(app) {
        let kind = ToolKind::of(&tool.name);
        let arg = |key: &str| {
            tool.args
                .get(key)
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_string()
        };
        let label = match kind {
            ToolKind::Execute => format!("Run {}", arg("command").trim()),
            ToolKind::Search => format!("Search {}", arg("pattern")),
            ToolKind::Fetch => format!("Fetch {}", arg("url")),
            ToolKind::WebSearch => format!("Web Search {}", arg("query")),
            ToolKind::Read => format!("Read {}", arg("path")),
            ToolKind::ListDir => format!("List {}", arg("path")),
            ToolKind::Edit => format!("Edit {}", arg("path")),
            ToolKind::Other => tool.name.clone(),
        };
        return (
            super::truncate_width(&label, 48),
            theme::style(Token::Success),
        );
    }
    let (_thinking, streaming) = app.transcript.streaming();
    if !streaming.is_empty() {
        return ("Responding\u{2026}".to_string(), secondary);
    }
    // Upstream distinguishes `Thinking…` (a model call in flight) from
    // `Waiting…`, its fallback for an activity it could not classify
    // (`turn_status.rs:680-739`). A Wizard turn that is busy with no tool
    // running and nothing streaming *is* the model call, so it takes the first;
    // there is no Wizard state left for the second, and inventing one to have
    // the string on screen would be quoting upstream rather than reporting.
    ("Thinking\u{2026}".to_string(), secondary)
}

/// The tool call the turn is currently inside, if any.
fn running_tool(app: &App) -> Option<&ToolItem> {
    app.transcript.iter().rev().find_map(|item| match item {
        TranscriptItem::Tool(tool) if tool.output.is_none() => Some(tool),
        _ => None,
    })
}

/// The states in which the agent is parked on the user.
///
/// Upstream's `is_pending_user_input` covers a permission prompt and
/// `ask_user_question`; Wizard's equivalents are the plan review, the interview
/// modal and a foreground command with the composer pointed at its stdin. The
/// drain-blocked wording is upstream's (`turn_status.rs:280-294`).
fn waiting_on_user(app: &App) -> Option<&'static str> {
    if app.plan_review.is_some() {
        return Some("waiting on your plan review");
    }
    if app.interview.is_some() {
        return Some("waiting on your answer");
    }
    if app.console.is_some() {
        return Some("agent idle ~ waiting on your input");
    }
    None
}

/// `1 command · 2 subagents still running`.
///
/// Ported from `format_still_running` (`P/src/views/turn_status.rs:132-167`):
/// the idle cue that a detached command or delegation has not finished, so a
/// backgrounded run never silently vanishes from the screen.
fn still_running(app: &App) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    let plural = |n: usize, one: &str| {
        if n == 1 {
            format!("1 {one}")
        } else {
            format!("{n} {one}s")
        }
    };
    if app.status.background_tasks > 0 {
        parts.push(plural(app.status.background_tasks, "command"));
    }
    if app.status.background_subagents > 0 {
        parts.push(plural(app.status.background_subagents, "subagent"));
    }
    (!parts.is_empty()).then(|| format!("{} still running", parts.join(" \u{00b7} ")))
}

// ---------------------------------------------------------------------------
// The top status bar
// ---------------------------------------------------------------------------

/// The right-aligned status bar above the scrollback.
///
/// Ported from `P/src/views/agent_status.rs:70-77` and the push order at
/// `P/src/app/agent_view/render.rs:1448-1560`: items joined by `" │ "` in
/// `gray_dim`, right-aligned, most transient first. The items themselves are
/// Wizard's, because this is Wizard's session — the background-task chip, the
/// context meter and the queue counter map straight onto upstream's; the mode
/// word, the provider warning and the vim indicator have no upstream
/// equivalent and are not dropped for looking foreign.
fn draw_status_bar(frame: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 || area.width < 8 {
        return;
    }
    let mut chips: Vec<Vec<Span<'static>>> = Vec::new();
    let dot = DOTS[(app.tick / SPINNER_DIVISOR) as usize % DOTS.len()];
    let spinner = SPINNER[(app.tick / SPINNER_DIVISOR) as usize % SPINNER.len()];

    if app.status.background_tasks > 0 {
        chips.push(vec![Span::styled(
            format!("{dot} {}", app.status.background_tasks),
            theme::style(Token::ToolRunning),
        )]);
    }
    if app.status.background_subagents > 0 {
        chips.push(vec![Span::styled(
            format!("{dot} {} sub", app.status.background_subagents),
            theme::style(Token::ToolRunning),
        )]);
    }
    if app.mcp_connecting {
        chips.push(vec![Span::styled(
            format!("{spinner} tools"),
            theme::style(Token::ToolRunning),
        )]);
    }
    if app.provider_health_error.is_some() {
        chips.push(vec![Span::styled(
            "\u{26a0} provider",
            super::warning().bold(),
        )]);
    }
    if let Some(label) = app.vim.label() {
        chips.push(vec![Span::styled(label, super::dim())]);
    }
    chips.push(vec![Span::styled(
        super::format_cwd(&app.project_root, 28),
        super::dim(),
    )]);
    if app.status.context_tokens > 0 {
        chips.push(vec![Span::styled(
            format_tokens_short(app.status.context_tokens),
            super::dim(),
        )]);
    }
    if !app.message_queue.is_empty() {
        chips.push(vec![Span::styled(
            format!("+{}", app.message_queue.len()),
            super::accent(),
        )]);
    }

    let mut spans: Vec<Span<'static>> = Vec::new();
    for (index, chip) in chips.into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(" \u{2502} ", super::dim()));
        }
        spans.extend(chip);
    }
    frame.render_widget(
        Paragraph::new(super::truncate_line(Line::from(spans), area.width as usize))
            .alignment(Alignment::Right),
        area,
    );
}

// ---------------------------------------------------------------------------
// The composer
// ---------------------------------------------------------------------------

/// The composer box, painted cell by cell.
///
/// Ported from `P/src/views/prompt_widget/mod.rs:2952-3253`. It is not a
/// `ratatui::widgets::Block` for one reason that matters: the session title is
/// inlined in the top border and the info line is painted *on* the bottom
/// border row, its leading and trailing spaces blanking the `─` underneath so
/// the text sits in a notch. A `Block` cannot do the second of those.
fn draw_composer(frame: &mut Frame, app: &App, area: Rect) {
    if area.width < 8 || area.height < 3 {
        return;
    }
    let focused = !super::overlay_open(app);
    // Idle versus active is *only* the border colour (`mod.rs:2916-2920`), plus
    // plan mode overriding it outright (`render.rs:994-998`).
    let border = if app.plan_mode || app.omakase {
        theme::style(Token::Warning)
    } else if focused {
        theme::style(Token::Border)
    } else {
        fade(Token::Border, 0.5)
    };

    let left = area.x;
    let right = area.right() - 1;
    let bottom = area.bottom() - 1;
    {
        let buf = frame.buffer_mut();
        for x in area.x..area.right() {
            let (top_glyph, bottom_glyph) = if x == left {
                ("\u{256d}", "\u{2570}") // ╭ ╰
            } else if x == right {
                ("\u{256e}", "\u{256f}") // ╮ ╯
            } else {
                ("\u{2500}", "\u{2500}") // ─
            };
            if let Some(cell) = buf.cell_mut(Position::new(x, area.y)) {
                cell.set_symbol(top_glyph).set_style(border);
            }
            if let Some(cell) = buf.cell_mut(Position::new(x, bottom)) {
                cell.set_symbol(bottom_glyph).set_style(border);
            }
        }
        for y in area.y + 1..bottom {
            for x in [left, right] {
                if let Some(cell) = buf.cell_mut(Position::new(x, y)) {
                    cell.set_symbol("\u{2502}").set_style(border); // │
                }
            }
        }
    }

    // The session title, right-aligned in the top border ending 3 cells before
    // the `╮`, in the shared chrome-caption style — the same one the info line
    // uses, so both borders read as one chrome (`mod.rs:2984-3004`, `:3361`).
    let title = session_title(app);
    if !title.is_empty() && area.width > 12 {
        let label = format!(
            " {} ",
            super::truncate_width(&title, area.width as usize - 8)
        );
        let width = label.width() as u16;
        let x = area.right().saturating_sub(3 + width);
        frame
            .buffer_mut()
            .set_string(x, area.y, label, caption_style(focused));
    }

    // The content area: inside the pads, which the border is painted over.
    let inner = Rect {
        x: area.x + PAD_LEFT,
        y: area.y + 1,
        width: area.width.saturating_sub(PAD_LEFT + PAD_RIGHT),
        height: area.height.saturating_sub(2),
    };
    draw_draft(frame, app, inner, focused);
    draw_info_line(
        frame,
        app,
        Rect {
            y: bottom,
            height: 1,
            ..inner
        },
        focused,
    );
}

/// The caption style shared by the session title and the info line's model
/// name: secondary text faded toward the background, further when unfocused.
///
/// Ported from `chrome_caption_style` (`P/src/views/prompt_widget/mod.rs:3361-3369`).
fn caption_style(focused: bool) -> Style {
    fade(Token::Muted, if focused { 0.6 } else { 0.4 })
}

/// What the composer's top border is titled with.
fn session_title(app: &App) -> String {
    if !app.session_name.is_empty() {
        return app.session_name.clone();
    }
    app.project_root
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The draft itself, plus the `❯ ` prefix and the caret.
fn draw_draft(frame: &mut Frame, app: &App, inner: Rect, focused: bool) {
    if inner.width < 4 || inner.height == 0 {
        return;
    }
    let budget = composer_budget(inner.width + PAD_LEFT + PAD_RIGHT);
    let chars = super::composer_chars(app);
    let cursor = app.cursor.min(chars.len());
    let normal = app.vim.is_normal();
    let rows = super::wrap_rows(&chars, budget);
    let (crow, ccol) = super::cursor_visual(&rows, cursor);

    let height = inner.height as usize;
    let voff = crow.saturating_sub(height.saturating_sub(1));
    let last = (voff + height).min(rows.len());
    let block = Style::default().add_modifier(Modifier::REVERSED);

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut caret: Option<(u16, u16)> = None;
    for (index, &(start, end)) in rows.iter().enumerate().take(last).skip(voff) {
        let row: &[char] = &chars[start..end];
        // Prefix precedence, upstream's (`mod.rs:3009-3028`) with Wizard's own
        // console mode standing in for bash mode: a different glyph in a
        // different colour, because the line goes somewhere else entirely.
        // Only the draft's *first* row carries it; wrapped and hard-broken
        // continuations indent to match, which is what keeps the text in one
        // column.
        let prefix = if index > 0 {
            Span::raw("  ")
        } else if app.console.is_some() {
            Span::styled("\u{25b6} ", super::warning().bold())
        } else if app.plan_mode || app.omakase {
            Span::styled(PROMPT_ARROW, theme::style(Token::Warning))
        } else {
            Span::styled(PROMPT_ARROW, super::accent())
        };
        let mut spans = vec![prefix];
        if normal && index == crow {
            let at = ccol.min(row.len());
            spans.push(Span::raw(row[..at].iter().collect::<String>()));
            if at < row.len() {
                spans.push(Span::styled(row[at].to_string(), block));
                spans.push(Span::raw(row[at + 1..].iter().collect::<String>()));
            } else {
                spans.push(Span::styled(" ", block));
            }
        } else {
            spans.push(Span::styled(
                row.iter().collect::<String>(),
                theme::style(Token::Text),
            ));
            if index == crow && !normal {
                let used: usize = row[..ccol.min(row.len())]
                    .iter()
                    .map(|ch| unicode_width::UnicodeWidthChar::width(*ch).unwrap_or(0))
                    .sum();
                caret = Some((inner.x + 2 + used as u16, inner.y + (index - voff) as u16));
            }
        }
        lines.push(Line::from(spans));
    }

    // The placeholder shows only when the box is empty *and* unfocused
    // (`mod.rs:3197-3213`). Upstream's is "Build anything"; this is Wizard's
    // own invitation, because the placeholder is copy, not chrome.
    if chars.is_empty() && !focused {
        lines = vec![Line::from(vec![
            Span::styled(PROMPT_ARROW, super::accent()),
            Span::styled("type a message", theme::style(Token::Muted)),
        ])];
    }

    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
    if focused
        && !normal
        && app.picker.is_none()
        && app.plan_review.is_none()
        && app.interview.is_none()
        && let Some((x, y)) = caret
    {
        frame.set_cursor_position(Position::new(x, y));
    }
}

/// The info line, painted **into** the bottom border row.
///
/// Ported from `render_info_line` (`P/src/views/prompt_widget/mod.rs:3381-3482`).
/// The whole trick is the leading and trailing `" "` pads: they are painted in
/// the same style as the text, so they blank the `─` cells the border pass
/// already wrote and the label appears to sit in a notch cut out of the box.
/// Everything is right-aligned; a `multiline` flag, when there is one, is
/// pinned further right still with a one-column gap.
fn draw_info_line(frame: &mut Frame, app: &App, area: Rect, focused: bool) {
    if area.width < 8 {
        return;
    }
    let sep = || Span::styled(" \u{00b7} ", super::dim());
    let mut left: Vec<Span<'static>> = vec![Span::raw(" ")];

    // Leftmost: the usage warning, upstream's slot for a credit limit. Wizard's
    // equivalent is a provider that failed its health probe.
    if app.provider_health_error.is_some() {
        left.push(Span::styled("provider unreachable", super::warning()));
        left.push(sep());
    }
    left.push(Span::styled(
        app.status.model.clone(),
        caption_style(focused),
    ));

    // Flags, in upstream's precedence: plan first, then the rest. These are
    // Wizard's own modes — the skin borrows a shape, never a feature set — so
    // fusion and ultra keep their names and their loudness.
    let flag = |bold: bool| {
        if bold {
            theme::style(Token::Accent).bold()
        } else {
            fade(Token::Muted, if focused { 0.75 } else { 0.5 })
        }
    };
    if app.omakase {
        left.push(sep());
        left.push(Span::styled("omakase", theme::style(Token::Warning)));
    } else if app.plan_mode {
        left.push(sep());
        left.push(Span::styled("plan", theme::style(Token::Warning)));
    }
    if app.fusion_active {
        left.push(sep());
        left.push(Span::styled("fusion", flag(true)));
    }
    if let Some(ultra) = &app.ultra {
        left.push(sep());
        left.push(Span::styled(
            format!("ULTRA \u{00d7}{}", ultra.candidates()),
            flag(true),
        ));
    }
    left.push(sep());
    left.push(super::mode_span(app.status.mode));
    left.push(Span::raw(" "));

    let right = app.input.contains('\n').then(|| {
        Line::from(vec![
            Span::styled("multiline", theme::style(Token::Muted)),
            Span::raw(" "),
        ])
    });

    let left = super::truncate_line(Line::from(left), area.width as usize);
    let buf = frame.buffer_mut();
    match right {
        Some(right) => {
            let right_width = right.width() as u16;
            let left_width = (left.width() as u16).min(area.width.saturating_sub(right_width + 1));
            let x = area
                .x
                .saturating_add(area.width.saturating_sub(left_width + 1 + right_width));
            buf.set_line(x, area.y, &left, left_width);
            buf.set_line(
                area.x + area.width.saturating_sub(right_width),
                area.y,
                &right,
                right_width,
            );
        }
        None => {
            let width = (left.width() as u16).min(area.width);
            buf.set_line(
                area.x + area.width.saturating_sub(width),
                area.y,
                &left,
                width,
            );
        }
    }
}

// ---------------------------------------------------------------------------
// The tasks pane
// ---------------------------------------------------------------------------

/// Rows of the tasks pane a group of `n` items may take before it is capped.
///
/// Upstream's pane scrolls (`P/src/views/list_pane/`); this one is a reserved
/// band above the shortcuts bar, so it caps instead and says how many it hid.
const GROUP_MAX_ROWS: usize = 5;

/// The chevron on a group header, expanded and collapsed.
///
/// Ported from `P/src/views/tasks_pane.rs:641-642`. The glyph plus its space is
/// exactly the width of an item's two-column indent, which is what makes the
/// labels line up down the pane.
const CHEVRON_OPEN: &str = "\u{25be} "; // ▾
const CHEVRON_SHUT: &str = "\u{25b8} "; // ▸

/// Rows the tasks pane needs, or 0 when nothing is delegated or detached.
fn tasks_height(app: &App) -> u16 {
    let mut rows = 0usize;
    if !app.panes.is_empty() {
        rows +=
            1 + app.panes.len().min(GROUP_MAX_ROWS) + usize::from(app.panes.len() > GROUP_MAX_ROWS);
    }
    let tasks = background_tasks(app);
    if !tasks.is_empty() {
        rows += 1 + tasks.len().min(GROUP_MAX_ROWS) + usize::from(tasks.len() > GROUP_MAX_ROWS);
    }
    rows as u16
}

/// Background commands still worth showing: everything running, and anything
/// that finished recently enough that its row has not been read yet.
fn background_tasks(app: &App) -> Vec<Task> {
    let Some(registry) = app.tasks.as_ref() else {
        return Vec::new();
    };
    registry
        .list()
        .into_iter()
        .filter(|task| {
            task.status == TaskStatus::Running
                || task
                    .finished
                    .is_some_and(|at| at.elapsed() < Duration::from_secs(30))
        })
        .collect()
}

/// Grok Build's tasks pane: background commands *and* delegated subagents in
/// one list, under collapsible group headers.
///
/// Ported from `P/src/views/tasks_pane.rs` (with `P/src/views/agents_modal.rs`
/// for the agent row's grammar). This is what replaced Wizard's own subagent
/// rail: upstream has no rail, it has one pane that answers "what is still
/// going" for every kind of detached work at once, laid out in the block's own
/// column geometry (`tasks_pane.rs:1277-1286` computes exactly
/// `ACCENT + block_pad_left` / `block_pad_right`), items indented two columns
/// under a `▾ Group N` header, the elapsed clock right-aligned, and the
/// selected row lifted onto `bg_highlight`.
///
/// What is Wizard's and stays Wizard's: the *selection* is `rail_focus` /
/// `attached`, so ↓ from the composer still walks these rows and Enter still
/// opens a run's own transcript. Losing that to look more like the thing being
/// imitated would be dropping a feature, not changing a skin.
fn draw_tasks_pane(frame: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 || area.width < 12 {
        return;
    }
    // The pane sits in the block's columns: one for the (unpainted) rail, two
    // of left pad, and two held back at the right.
    let inner = Rect {
        x: area.x + ACCENT + PAD_LEFT,
        width: area.width.saturating_sub(CHROME_WIDTH),
        ..area
    };
    if inner.width < 8 {
        return;
    }
    let focused = app.rail_focus;
    let selected = focused.or(app.attached);
    let width = inner.width as usize;

    let mut rows: Vec<Line<'static>> = Vec::new();
    let mut selected_row: Option<usize> = None;

    if !app.panes.is_empty() {
        rows.push(group_header("Subagents", app.panes.len()));
        // Scroll the window so the selection stays on screen once there are
        // more runs than rows.
        let visible = app.panes.len().min(GROUP_MAX_ROWS);
        let start = match selected {
            Some(index) if index >= visible => index + 1 - visible,
            _ => 0,
        };
        for (index, pane) in app.panes.iter().enumerate().skip(start).take(visible) {
            if selected == Some(index) {
                selected_row = Some(rows.len());
            }
            rows.push(agent_row(
                pane,
                width,
                focused.is_some() && selected == Some(index),
                app.attached == Some(index),
            ));
        }
        if app.panes.len() > visible {
            rows.push(overflow_row(app.panes.len() - visible));
        }
    }

    let tasks = background_tasks(app);
    if !tasks.is_empty() {
        rows.push(group_header("Tasks", tasks.len()));
        for task in tasks.iter().take(GROUP_MAX_ROWS) {
            rows.push(task_row(task, width));
        }
        if tasks.len() > GROUP_MAX_ROWS {
            rows.push(overflow_row(tasks.len() - GROUP_MAX_ROWS));
        }
    }

    let height = rows.len().min(area.height as usize);
    frame.render_widget(
        Paragraph::new(Text::from(
            rows.into_iter().take(height).collect::<Vec<_>>(),
        )),
        inner,
    );

    // The selection is a lifted row, upstream's `selection_bg` — painted over
    // the *whole* band including the chrome columns, which is what makes it
    // read as a cursor line rather than as highlighted text.
    if let Some(row) = selected_row.filter(|row| *row < height)
        && let Some(bg) = Tint::Raised.resolve()
    {
        let y = inner.y + row as u16;
        let buf = frame.buffer_mut();
        for x in area.x..area.right() {
            if let Some(cell) = buf.cell_mut(Position::new(x, y)) {
                cell.set_bg(bg);
            }
        }
    }
}

/// `▾ Subagents 2` — a chevron, a bold label, a count.
///
/// Ported from `P/src/views/tasks_pane.rs:640-654`.
fn group_header(label: &'static str, count: usize) -> Line<'static> {
    Line::from(vec![
        Span::styled(CHEVRON_OPEN, super::dim()),
        Span::styled(label, theme::style(Token::Muted).bold()),
        Span::styled(format!(" {count}"), super::dim()),
    ])
}

/// The row a capped group ends on, in the collapsed chevron so it reads as
/// "there is more behind this".
fn overflow_row(hidden: usize) -> Line<'static> {
    Line::from(vec![
        Span::styled(CHEVRON_SHUT, super::dim()),
        Span::styled(format!("{hidden} more"), super::dim().italic()),
    ])
}

/// One subagent: `  Researcher map the auth flow — Thinking     0.4s`.
///
/// Ported from `P/src/views/tasks_pane.rs:376-445`: a capitalized type label
/// coloured by state (running, completed, failed — blended 0.45 toward the
/// background once it is no longer running), the description in body text while
/// running and gray once it is not, a live activity suffix after an em dash,
/// and the elapsed clock right-aligned as its own overlay.
fn agent_row(pane: &SubagentPane, width: usize, cursor: bool, attached: bool) -> Line<'static> {
    let (token, running) = match pane.status {
        PaneStatus::Running => (Token::ToolRunning, true),
        PaneStatus::Done => (Token::ToolDone, false),
        PaneStatus::Failed => (Token::ToolFailed, false),
    };
    let type_style = if running {
        theme::style(token)
    } else {
        fade(token, 0.45)
    };
    // Bold on failure, for the reason every ✗ in this codebase is bold: under a
    // monochrome theme the token alone is one gray among others.
    let type_style = if pane.status == PaneStatus::Failed {
        type_style.add_modifier(Modifier::BOLD)
    } else {
        type_style
    };
    let text = if running {
        theme::style(Token::Text)
    } else {
        theme::style(Token::Muted)
    };
    let name = capitalize(&pane.name);
    let elapsed = format!("{} ", format_duration(pane.elapsed()));

    let mut spans = vec![
        // The cursor mark takes the same two columns an item's indent does, so
        // nothing shifts when the rail gains or loses focus.
        Span::styled(if cursor { "\u{203a} " } else { "  " }, super::accent()),
        Span::styled(format!("{name} "), type_style),
    ];
    let mut used = 2 + name.width() + 1;
    let activity = live_activity(pane);
    // Room for the description, the activity suffix, the elapsed overlay, and
    // the unread badge — the description yields first, as upstream's does.
    // What this run did while you were looking somewhere else. Suppressed
    // while it is the run you are inside, where there is nothing unread.
    let unread = if pane.unread > 0 && !attached {
        format!(" +{}", pane.unread)
    } else {
        String::new()
    };
    let reserved = elapsed.width() + unread.width() + 1;
    let room = width.saturating_sub(used + reserved);
    let described = if running && !activity.is_empty() {
        let description =
            super::truncate_width(&pane.task, room.saturating_sub(activity.width() + 3).max(8));
        used += description.width();
        spans.push(Span::styled(description, text));
        spans.push(Span::styled(format!(" \u{2014} {activity}"), super::dim()));
        used += activity.width() + 3;
        used
    } else {
        let description = super::truncate_width(&pane.task, room.max(8));
        used += description.width();
        spans.push(Span::styled(description, text));
        used
    };
    if !unread.is_empty() {
        spans.push(Span::styled(unread.clone(), super::accent().bold()));
    }
    let gap = width.saturating_sub(described + unread.width() + elapsed.width());
    spans.push(Span::raw(" ".repeat(gap)));
    spans.push(Span::styled(elapsed, super::dim()));
    super::truncate_line(Line::from(spans), width)
}

/// One background command: `  exit 0 cargo build --release          12s`.
///
/// Ported from `P/src/views/tasks_pane.rs:267-330` (the bg-task arm) — the same
/// row shape as an agent's, with the exit status standing in for the persona.
fn task_row(task: &Task, width: usize) -> Line<'static> {
    let (label, token) = match task.status {
        TaskStatus::Running => ("run".to_string(), Token::ToolRunning),
        TaskStatus::Done(0) => ("exit 0".to_string(), Token::ToolDone),
        TaskStatus::Done(code) => (format!("exit {code}"), Token::ToolFailed),
        TaskStatus::Killed => ("killed".to_string(), Token::ToolFailed),
        TaskStatus::TimedOut => ("timed out".to_string(), Token::ToolFailed),
    };
    let running = task.status == TaskStatus::Running;
    let style = if running {
        theme::style(token)
    } else {
        fade(token, 0.45)
    };
    let elapsed = format!(
        "{} ",
        format_duration(
            task.finished
                .map_or_else(|| task.started.elapsed(), |at| at - task.started)
        )
    );
    let head = format!("  {label} ");
    let room = width.saturating_sub(head.width() + elapsed.width() + 1);
    let command = super::truncate_width(task.command.trim(), room.max(8));
    let gap = width.saturating_sub(head.width() + command.width() + elapsed.width());
    super::truncate_line(
        Line::from(vec![
            Span::styled(head, style),
            Span::styled(
                command,
                if running {
                    theme::style(Token::Text)
                } else {
                    theme::style(Token::Muted)
                },
            ),
            Span::raw(" ".repeat(gap)),
            Span::styled(elapsed, super::dim()),
        ]),
        width,
    )
}

/// `researcher` → `Researcher`: upstream labels a subagent by its capitalized
/// persona (`Explore`, `Plan`, `General`), not by its raw id.
fn capitalize(name: &str) -> String {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

// ---------------------------------------------------------------------------
// The todo pane
// ---------------------------------------------------------------------------

/// Rows the todo pane needs, capped so a long list cannot swallow the
/// transcript, and 0 when it is hidden.
fn todos_height(app: &App, area: Rect, below: u16) -> u16 {
    if !app.show_todos {
        return 0;
    }
    let wanted = app.todos.len().max(1) as u16;
    // One row of transcript always survives, whatever the list does.
    let spare = area.height.saturating_sub(below + 1);
    wanted.min(spare).min(12)
}

/// The todo list, in Grok Build's own pane grammar.
///
/// Ported from `P/src/views/todo_pane.rs:38-124`: the icons are `□` pending,
/// `▶` in progress, `✓` done and `✗` cancelled; in-progress rows are
/// `theme.warning` + bold, done rows take a green icon over gray text, and
/// cancelled rows are struck through. There is no border and **no header
/// row** — `TodoPane::render` (`todo_pane.rs:486-522`) puts a bare `ListPane`
/// into the block's own columns and nothing else. The `▾ Group N` header
/// belongs to the *tasks* pane (`tasks_pane.rs:637-653`), which is a different
/// pane; borrowing it for the todos was the mistake.
fn draw_todos(frame: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 || area.width < 12 {
        return;
    }
    let inner = Rect {
        x: area.x + ACCENT + PAD_LEFT,
        width: area.width.saturating_sub(CHROME_WIDTH),
        ..area
    };
    if inner.width < 8 {
        return;
    }
    let mut rows: Vec<Line<'static>> = Vec::new();

    if app.todos.is_empty() {
        // `empty_placeholder_message` (`todo_pane.rs:497-506`): a plain line in
        // `gray_bright` at the pane's own origin, not an indented parenthetical.
        rows.push(Line::from(Span::styled("No todo items.", super::muted())));
    } else {
        // Prefer the in-progress item and its neighbours when the list is
        // taller than the band: the current work stays visible.
        let visible = (area.height as usize).max(1);
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
        rows.extend(
            app.todos
                .iter()
                .skip(start)
                .take(visible)
                .map(|item| todo_row(item, inner.width as usize)),
        );
    }

    let height = rows.len().min(area.height as usize);
    frame.render_widget(
        Paragraph::new(Text::from(
            rows.into_iter().take(height).collect::<Vec<_>>(),
        )),
        inner,
    );
}

/// One todo row: a status icon, a space, and the text.
///
/// Ported from `P/src/views/todo_pane.rs:103-124`. The icons carry the meaning
/// on their own, which is the house rule and happens also to be what upstream
/// does here — unlike its tool bullets.
///
/// `TodoPaneStyle::default` (`todo_pane.rs:34-61`) is the whole table, and two
/// of its rows are easy to get wrong: a **pending** item is `text_primary`, the
/// same weight as any other prose, not muted; and a **completed** one is
/// `gray_bright` and nothing more. The strike-through belongs to `Cancelled`,
/// which Wizard's todo model has no state for, so nothing here is struck.
fn todo_row(item: &TodoItem, width: usize) -> Line<'static> {
    let (icon, icon_style, text_style) = match item.status {
        TodoStatus::Pending => (
            "\u{25a1}",
            theme::style(Token::Text),
            theme::style(Token::Text),
        ),
        TodoStatus::InProgress => (
            "\u{25b6}",
            theme::style(Token::Warning),
            theme::style(Token::Text).bold(),
        ),
        TodoStatus::Completed => ("\u{2713}", theme::style(Token::Success), super::muted()),
    };
    super::truncate_line(
        Line::from(vec![
            Span::styled(format!("{icon} "), icon_style),
            Span::styled(item.content.clone(), text_style),
        ]),
        width,
    )
}

// ---------------------------------------------------------------------------
// The shortcuts bar
// ---------------------------------------------------------------------------

/// The row below the composer: `key:label` pairs separated by `"  │  "`.
///
/// Ported from `P/src/views/shortcuts_bar.rs:212-320`: keys in
/// `text_secondary` + bold, labels and separator in `gray` (the separator
/// additionally dim). The pairs are contextual, and `send` becomes `queue`
/// while a turn is running exactly as upstream does (`P/src/views/agent.rs:999`)
/// — which is also true of Wizard's Enter, so the swap is not cosmetic.
///
/// The key *names* are `KeyShortcut::display()`'s
/// (`P/src/input/key.rs:80-140`), which is where the casing comes from and why
/// it is not the lowercase Wizard writes everywhere else: modifiers are
/// `Ctrl+` / `Alt+` / `Shift+`, a plain character stays lowercase, and the
/// named keys are `Enter`, `Esc`, `Tab`, `Bsp`, `Del`, `PgUp`, `PgDn`, `F<n>`,
/// with the arrows as glyphs. `Shift+Tab` is one atom (`KeyCode::BackTab`),
/// not a modifier over `Tab`.
fn draw_shortcuts(frame: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 || area.width < 8 {
        return;
    }
    let pairs: &[(&str, &str)] = if app.plan_review.is_some() {
        &[
            ("y", "approve"),
            ("n", "reject"),
            ("\u{2191}\u{2193}", "scroll"),
            ("Esc", "back"),
        ]
    } else if app.interview.is_some() {
        &[("1-9", "pick"), ("Enter", "next"), ("Esc", "skip")]
    } else if app.picker.is_some() {
        &[
            ("\u{2191}\u{2193}", "move"),
            ("Enter", "select"),
            ("Esc", "cancel"),
        ]
    } else if !app.suggestions.is_empty() {
        &[
            ("\u{2191}\u{2193}", "select"),
            ("Tab", "complete"),
            ("Enter", "run"),
        ]
    } else if app.diff.is_some() {
        &[("PgUp/PgDn", "diff"), ("Esc", "close")]
    } else if app.console.is_some() {
        &[
            ("Enter", "command"),
            ("Ctrl+d", "end input"),
            ("Esc", "detach"),
            ("Ctrl+c", "stop"),
        ]
    } else if app.status.busy {
        &[
            ("Enter", "queue"),
            ("Shift+Enter", "newline"),
            ("Ctrl+c", "stop"),
            ("PgUp/PgDn", "scroll"),
        ]
    } else {
        &[
            ("Enter", "send"),
            ("Shift+Enter", "newline"),
            ("/", "commands"),
            ("Ctrl+t", "expand"),
            ("Shift+Tab", "mode"),
        ]
    };

    let key = theme::style(Token::Muted).bold();
    let label = super::dim();
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (index, (name, what)) in pairs.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(
                "  \u{2502}  ",
                super::dim().add_modifier(Modifier::DIM),
            ));
        }
        spans.push(Span::styled(*name, key));
        spans.push(Span::styled(":", label));
        spans.push(Span::styled(*what, label));
    }
    frame.render_widget(
        Paragraph::new(super::truncate_line(Line::from(spans), area.width as usize)),
        area,
    );
}

// ---------------------------------------------------------------------------
// The welcome screen
// ---------------------------------------------------------------------------

/// Wizard's wand-and-spark mark, as braille dots.
///
/// The same art as `crate::ui::welcome`'s, which is private to that module and
/// not reachable from here; it is duplicated rather than the shared file being
/// widened, per the brief. Every pad cell is U+2800 BRAILLE PATTERN BLANK, not
/// an ASCII space — the same rule upstream's `assets/logo/logo07.txt` follows —
/// so the rows are all the same *cell* width whatever the terminal does with
/// trailing whitespace.
///
/// It is Wizard's mark and not Grok Build's: the shape of the welcome screen is
/// borrowed, the identity on it is not.
const MARK: &[&str] = &[
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢳⡀⠀⠀⠀⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠸⣿⣦⣀⣤⣤⡴⠂",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⣠⣿⣿⣿⡿⠋⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⡠⠾⠟⠛⠻⣿⣷⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢀⣤⣶⡄⠀⠀⠀⠈⠻⡄⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢀⣴⣿⡿⠋⠀⠀⠀⠀⠀⠀⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢀⣴⣿⡿⠋⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠀⢀⣴⣿⡿⠋⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⢀⣴⣿⡿⠋⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
    "⠀⠀⠀⠀⢀⣴⣿⣿⠟⠁⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
    "⠀⠀⢀⣴⣿⣿⠟⠁⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
    "⢀⣴⣿⣿⠟⠁⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
    "⢿⣿⠟⠁⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
];

/// Columns below which the welcome screen drops its hero box.
///
/// Ported from `P/src/views/welcome/hero_box.rs:14` (`HERO_BOX_MIN_WIDTH = 90`).
const HERO_BOX_MIN_WIDTH: u16 = 90;

/// Rows below which the mark is hidden entirely.
///
/// Ported from `P/src/views/welcome/logo.rs:17` (`SMALL_LOGO_MIN_HEIGHT = 22`).
const LOGO_MIN_HEIGHT: u16 = 22;

/// Columns the welcome menu's label/key column spans.
///
/// Ported from `P/src/views/welcome/mod.rs:84` (`MENU_MIN_WIDTH = 51`). Every
/// row is padded to it, so the keys land in one column whether the block is
/// centred (the stacked layout) or flush left (the hero box).
const MENU_WIDTH: usize = 51;

/// The home screen: a hero box at 90 columns and up, a stacked column below it.
///
/// Ported in shape from `P/src/views/welcome/`. The hero box is the only real
/// `ratatui::widgets::Block` in the whole of Grok Build's UI
/// (`hero_box.rs:317-324`, rounded borders in a border colour blended most of
/// the way to the background), with the logo on the left and the version,
/// subtitle and menu on the right.
fn draw_welcome(frame: &mut Frame, app: &App, area: Rect) {
    if area.width >= HERO_BOX_MIN_WIDTH && area.height >= LOGO_MIN_HEIGHT {
        draw_hero_box(frame, app, area);
    } else {
        draw_welcome_stacked(frame, app, area);
    }
}

/// The right-hand column of the welcome screen: who this is, what it is running
/// on, anything that went wrong at startup, and how to begin.
///
/// The last of those is not decoration: a home screen that dropped a startup
/// warning to look tidier would be hiding the one thing on it that needs
/// acting on.
fn welcome_body(app: &App) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(vec![
            Span::styled("Wizard  ", theme::style(Token::Text).bold()),
            Span::styled(env!("CARGO_PKG_VERSION"), super::dim()),
        ]),
        Line::from(Span::styled("your sovereign agent", super::dim().italic())),
        Line::raw(""),
        Line::from(vec![
            super::model_span(app),
            Span::styled(" \u{00b7} ", super::dim()),
            super::mode_span(app.status.mode),
        ]),
        Line::from(Span::styled(
            super::format_cwd(&app.project_root, 48),
            super::dim(),
        )),
        Line::raw(""),
    ];
    if let Some(err) = &app.provider_health_error {
        lines.push(Line::from(Span::styled(
            super::truncate_width(&format!("\u{26a0} provider unreachable: {err}"), 68),
            super::warning().bold(),
        )));
    }
    for item in app.transcript.iter().rev().take(3) {
        if let TranscriptItem::Notice(text) = item {
            lines.push(Line::from(Span::styled(
                format!(
                    "\u{26a0} {}",
                    super::truncate_width(text.lines().next().unwrap_or(""), 68)
                ),
                super::warning(),
            )));
        }
    }
    if lines.last().is_some_and(|line| line.width() > 0) {
        lines.push(Line::raw(""));
    }
    // The menu, upstream's shape: a label flush left and its key flush right in
    // a column of a fixed width (`P/src/views/welcome/menu.rs:36-53`, with
    // `MENU_MIN_WIDTH`), which is why the rows are padded out to that width
    // rather than centred one at a time — a menu whose keys do not line up in
    // a column has stopped being a menu. Labels take `text_primary` + bold,
    // keys `gray_bright` (`menu.rs:23-33`). The commands are Wizard's: a skin
    // never renames one.
    for (label, key) in [
        ("Pick a model", "/model"),
        ("Switch the interface", "/ui"),
        ("All commands & keys", "/help"),
        ("Quit", "Ctrl+c"),
    ] {
        let gap = MENU_WIDTH.saturating_sub(label.width() + key.width());
        lines.push(Line::from(vec![
            Span::styled(label, theme::style(Token::Text).bold()),
            Span::raw(" ".repeat(gap)),
            Span::styled(key, theme::style(Token::Muted)),
        ]));
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        "Type a message to begin.",
        super::dim(),
    )));
    lines
}

/// The ≥90-column form: a rounded box, the mark on the left, the block on the
/// right.
fn draw_hero_box(frame: &mut Frame, app: &App, area: Rect) {
    use ratatui::widgets::{Block, BorderType, Borders};

    let lines = welcome_body(app);
    let mark_width = MARK.first().map_or(0, |row| row.chars().count()) as u16;
    // `content_area.width - 6`, capped at 120 (`hero_box.rs:117`).
    let width = area.width.saturating_sub(6).min(120);
    let height = (lines.len() as u16 + 2)
        .max(MARK.len() as u16 + 2)
        .min(area.height);
    let outer = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    // A border most of the way blended into the background: present, but not a
    // line anyone looks at (`hero_box.rs:317-324`, blend 0.45).
    let block = Block::new()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(fade(Token::Border, 0.45));
    let inner = block.inner(outer);
    frame.render_widget(block, outer);

    // `LOGO_H_PAD = 3` either side of the mark (`hero_box.rs:25`).
    let [logo, body] =
        Layout::horizontal([Constraint::Length(mark_width + 6), Constraint::Min(10)]).areas(inner);
    frame.render_widget(
        Paragraph::new(Text::from(
            MARK.iter()
                .map(|row| Line::from(Span::styled(*row, shimmer_style(app.tick))))
                .collect::<Vec<_>>(),
        ))
        .alignment(Alignment::Center),
        logo,
    );
    frame.render_widget(Paragraph::new(Text::from(lines)), body);
}

/// The below-90-column form: the mark, a gap, then the block, centred as a
/// column (`P/src/views/welcome/mod.rs:345-376`).
fn draw_welcome_stacked(frame: &mut Frame, app: &App, area: Rect) {
    let mut lines: Vec<Line<'static>> = Vec::new();
    if area.height >= LOGO_MIN_HEIGHT {
        lines.extend(
            MARK.iter()
                .map(|row| Line::from(Span::styled(*row, shimmer_style(app.tick)))),
        );
        lines.push(Line::raw(""));
    }
    lines.extend(welcome_body(app));
    let height = (lines.len() as u16).min(area.height);
    frame.render_widget(
        Paragraph::new(Text::from(lines)).alignment(Alignment::Center),
        Rect {
            y: area.y + area.height.saturating_sub(height) / 2,
            height,
            ..area
        },
    );
}

/// The logo's colour, breathing between the muted gray and body text.
///
/// Upstream sweeps a raised-cosine shine band diagonally across the glyphs on a
/// wall-clock 4-second cycle (`P/src/views/welcome/logo.rs:88-138`). That is a
/// per-glyph effect and the mark here is drawn a row at a time, so this keeps
/// the *cycle* — a slow breath between `theme.gray` and `theme.text_primary` —
/// and drops the spatial sweep, which is the part a row-wise renderer cannot
/// carry without one span per cell.
fn shimmer_style(tick: u64) -> Style {
    let brightness = motion::pulse(tick, 0.05);
    Style::default().fg(motion::breathe(theme::color(Token::Faint), brightness))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentEvent;
    use crate::config::Config;
    use crate::skin::Skin;

    fn app() -> App {
        let mut app = App::new(Config::default());
        app.welcome_dismissed = true;
        app
    }

    /// The palette this skin ships with, pinned to this thread. The whole
    /// suite writes the process-wide slot, so a test that swapped it would be
    /// asserting against a value other threads are changing underneath it.
    fn grok_theme() -> crate::theme::Pinned {
        crate::theme::pin(std::sync::Arc::new(
            crate::theme::load("grok").expect("the grok theme ships"),
        ))
    }

    /// Render at `width`×`height` under the `grok` skin, one string per row.
    fn render(app: &App, width: u16, height: u16) -> Vec<String> {
        let _pinned = crate::skin::pin(Skin::Grok);
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| super::super::draw(frame, app))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    fn text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    // ── layout maths ────────────────────────────────────────────────────────

    #[test]
    fn the_block_columns_are_grok_builds_own_widths() {
        // │A│PL│content│PR│ = 1 + 2 + flex + 2. The doc comment upstream says
        // the right pad is 1; the config says 2, and the config is what runs.
        assert_eq!(CHROME_WIDTH, 5);
        // At 80 columns the screen keeps 2 either side plus a scrollbar column,
        // leaving a 75-wide block whose content is 70 columns.
        assert_eq!(content_width(76), 71);
        assert_eq!(bulleted_width(76), 69, "the ◆ takes two more");
        // Never zero, however narrow the terminal.
        assert_eq!(content_width(1), 1);
        assert_eq!(bulleted_width(1), 1);
    }

    #[test]
    fn the_accent_rail_runs_the_blocks_whole_height_padding_included() {
        let entry = user_entry("hello", 40);
        let rows = decorate(&entry, 40, 0);
        // One padding row above, the prompt, one below.
        assert_eq!(rows.len(), 3);
        // The user prompt paints no rail of its own — the column is *cleared*,
        // which is a blank cell and not an absent one, so the prose still
        // starts three columns in.
        for row in &rows {
            let line = text(row);
            assert!(
                line.starts_with("   ") || line.trim().is_empty(),
                "the column is reserved: {line:?}"
            );
        }
        assert!(text(&rows[1]).starts_with("   \u{276f} hello"));
    }

    #[test]
    fn a_running_tool_paints_a_bar_and_a_finished_one_a_thinner_mark() {
        let running = tool_entry(
            &ToolItem {
                name: "execute".into(),
                args: serde_json::json!({ "command": "cargo test" }),
                call_id: String::new(),
                output: None,
                progress: String::new(),
            },
            &app(),
            false,
            0,
            60,
        );
        assert!(text(&decorate(&running, 60, 0)[0]).starts_with(RAIL));

        let done = tool_entry(
            &ToolItem {
                name: "read_file".into(),
                args: serde_json::json!({ "path": "src/main.rs" }),
                call_id: String::new(),
                output: Some(crate::transcript::ToolItemOutput {
                    content: "fn main() {}".into(),
                    is_error: false,
                }),
                progress: String::new(),
            },
            &app(),
            true,
            0,
            60,
        );
        // Collapsed and groupable: the thinner `❙`. Read never paints a rail at
        // all, so this one is a cleared column — assert the glyph rule directly
        // instead, on a block that does have one.
        assert_eq!(done.mode, Mode::Collapsed);
        let other = Entry {
            kind: Kind::Tool,
            mode: Mode::Collapsed,
            accent: Some(Token::Success),
            animated: false,
            pending: false,
            bullet: None,
            rows: vec![Row::plain(Line::raw("x"))],
            card: None,
            images: Vec::new(),
        };
        assert!(text(&decorate(&other, 60, 0)[0]).starts_with(RAIL_COLLAPSED));
    }

    #[test]
    fn only_the_user_prompt_gets_vertical_padding_and_a_slab() {
        assert_eq!(Kind::UserPrompt.vpad(), 1);
        assert!(Kind::UserPrompt.tint().is_some());
        for kind in [
            Kind::Agent,
            Kind::Thinking,
            Kind::Tool,
            Kind::Subagent,
            Kind::BgTask,
            Kind::System,
        ] {
            assert_eq!(kind.vpad(), 0, "{kind:?} overrides vpad away");
            assert!(
                kind.tint().is_none(),
                "{kind:?} overrides its background away"
            );
        }
    }

    #[test]
    fn the_prompt_band_covers_the_pads_and_the_padding_rows() {
        // The block background is the one thing a text dump cannot show, and
        // it is the whole of what a prompt band *is*: `bg_light` across the
        // accent column, both pads and the content, on every row of the entry
        // — the blank ones above and below included.
        let _theme = grok_theme();
        let rows = decorate(&user_entry("hello", 40), 40, 0);
        assert_eq!(rows.len(), 3, "one padding row each side of the prompt");
        let bg = Tint::Raised.resolve().expect("the grok theme declares one");
        for (index, row) in rows.iter().enumerate() {
            assert_eq!(
                row.width(),
                40,
                "row {index} is a rectangle, not the shape of its text"
            );
            for span in &row.spans {
                assert_eq!(span.style.bg, Some(bg), "row {index}: {:?}", span.content);
            }
        }
    }

    #[test]
    fn a_commands_output_sits_on_its_own_band_and_the_rest_of_the_block_does_not() {
        // Panel bands are per-*line*, not per-block: `bg_dark` behind the
        // output preview and nothing behind the header above it.
        let _theme = grok_theme();
        let entry = tool_entry(
            &ToolItem {
                name: "execute".into(),
                args: serde_json::json!({ "command": "cargo test" }),
                call_id: String::new(),
                output: Some(crate::transcript::ToolItemOutput {
                    content: "ok".into(),
                    is_error: false,
                }),
                progress: String::new(),
            },
            &app(),
            false,
            0,
            60,
        );
        let rows = decorate(&entry, 60, 0);
        let sunken = Tint::Sunken.resolve().expect("the grok theme declares one");
        assert!(
            rows[0].spans.iter().all(|span| span.style.bg.is_none()),
            "the header is unbanded: {:?}",
            text(&rows[0])
        );
        assert!(
            rows[1]
                .spans
                .iter()
                .any(|span| span.style.bg == Some(sunken)),
            "the output is banded: {:?}",
            text(&rows[1])
        );
    }

    // ── the grouping rule ───────────────────────────────────────────────────

    #[test]
    fn adjacent_collapsed_groupable_entries_pack_with_no_gap() {
        let collapsed_tool = || Entry {
            kind: Kind::Tool,
            mode: Mode::Collapsed,
            accent: Some(Token::Muted),
            animated: false,
            pending: false,
            bullet: None,
            rows: vec![Row::plain(Line::raw("x"))],
            card: None,
            images: Vec::new(),
        };
        let open_tool = || Entry {
            mode: Mode::Truncated,
            ..collapsed_tool()
        };
        let prose = || Entry {
            kind: Kind::Agent,
            mode: Mode::Collapsed,
            ..collapsed_tool()
        };

        assert_eq!(gap_after(&collapsed_tool(), &collapsed_tool()), 0);
        // One of them open: a gap row comes back.
        assert_eq!(gap_after(&collapsed_tool(), &open_tool()), 1);
        assert_eq!(gap_after(&open_tool(), &collapsed_tool()), 1);
        // An agent message is never groupable, whatever its mode says.
        assert_eq!(gap_after(&collapsed_tool(), &prose()), 1);
        assert_eq!(gap_after(&prose(), &prose()), 1);
        // Subagent and bg-task rows are groupable, and are always collapsed.
        let sub = Entry {
            kind: Kind::Subagent,
            ..collapsed_tool()
        };
        assert_eq!(gap_after(&sub, &collapsed_tool()), 0);
    }

    #[test]
    fn a_run_of_collapsed_tools_is_solid_and_an_open_one_breaks_it() {
        let tool = |name: &str, folded: bool| Entry {
            kind: Kind::Tool,
            mode: if folded {
                Mode::Collapsed
            } else {
                Mode::Expanded
            },
            accent: Some(Token::Muted),
            animated: false,
            pending: false,
            bullet: Some((DIAMOND, Token::Muted)),
            rows: vec![Row::plain(Line::from(Span::raw(name.to_string())))],
            card: None,
            images: Vec::new(),
        };
        let entries = vec![tool("a", true), tool("b", true), tool("c", false)];
        let out = flatten(&entries, 40, 0, Vec::new());
        let rows: Vec<String> = out.lines.iter().map(text).collect();
        // a, b, gap, c — three entries and exactly one blank row.
        assert_eq!(rows.len(), 4, "{rows:?}");
        assert!(rows[2].trim().is_empty(), "{rows:?}");
    }

    // ── the composer's notch ────────────────────────────────────────────────

    #[test]
    fn the_info_line_is_a_notch_cut_into_the_bottom_border() {
        let mut app = app();
        app.status.model = "wizard-1".to_string();
        let rows = render(&app, 80, 24);
        // Find the bottom border of the composer: the row with `╰` and `╯`.
        let bottom = rows
            .iter()
            .find(|row| row.contains('\u{2570}') && row.contains('\u{256f}'))
            .expect("the composer has a bottom border");
        assert!(
            bottom.contains("wizard-1"),
            "the model is inlined: {bottom}"
        );
        assert!(bottom.contains("genie"), "and so is the mode: {bottom}");
        // The pads either side blank the `─` they sit on, so the label is
        // surrounded by spaces rather than by dashes.
        let at = bottom.find("wizard-1").unwrap();
        assert_eq!(
            &bottom[at - 1..at],
            " ",
            "a leading space blanks the border under it: {bottom}"
        );
        // And it really is the border row: corners at both ends.
        assert!(bottom.starts_with("  \u{2570}"), "{bottom}");
        assert!(bottom.trim_end().ends_with('\u{256f}'), "{bottom}");
    }

    #[test]
    fn the_session_title_is_inlined_in_the_top_border() {
        let mut app = app();
        app.session_name = "fix the parser".to_string();
        let rows = render(&app, 80, 24);
        let top = rows
            .iter()
            .find(|row| row.contains('\u{256d}') && row.contains('\u{256e}'))
            .expect("the composer has a top border");
        assert!(top.contains("fix the parser"), "{top}");
        // Right-aligned, ending three cells before the ╮: one trailing pad
        // space (which blanks the `─` under it), then two plain border cells.
        // Counted in *cells*, not bytes — `─` is three bytes wide.
        let cells: Vec<char> = top.chars().collect();
        let corner = cells.iter().position(|ch| *ch == '\u{256e}').unwrap();
        let end = top
            .chars()
            .take(corner)
            .collect::<String>()
            .find("fix the parser")
            .is_some();
        assert!(end, "{top}");
        let tail: String = cells[corner - 3..=corner].iter().collect();
        assert_eq!(tail, " \u{2500}\u{2500}\u{256e}", "{top}");
    }

    #[test]
    fn the_composer_budget_leaves_room_for_both_borders_and_the_prompt() {
        // 76 columns of box: two border/pad columns each side, two for `❯ `.
        assert_eq!(composer_budget(76), 70);
        assert!(composer_budget(4) >= 1, "never zero");
    }

    // ── wording and formatting ──────────────────────────────────────────────

    #[test]
    fn durations_and_token_counts_read_the_way_grok_build_prints_them() {
        assert_eq!(format_duration(Duration::from_millis(200)), "0.2s");
        assert_eq!(format_duration(Duration::from_secs(42)), "42s");
        assert_eq!(format_duration(Duration::from_secs(80)), "1m20s");
        assert_eq!(format_duration(Duration::from_secs(8000)), "2h13m");
        assert_eq!(format_tokens_short(999), "999");
        assert_eq!(format_tokens_short(1_234), "1.23k");
        assert_eq!(format_tokens_short(12_345), "12.3k");
        assert_eq!(format_tokens_short(500_000), "500k");
        assert_eq!(format_tokens_short(1_234_567), "1.23m");
    }

    #[test]
    fn a_tool_header_reads_as_verb_operand_detail() {
        let read = ToolItem {
            name: "read_file".into(),
            args: serde_json::json!({ "path": "src/app/cli.rs", "start_line": 1, "end_line": 120 }),
            call_id: String::new(),
            output: None,
            progress: String::new(),
        };
        let header = text(&Line::from(tool_header(
            &read,
            ToolKind::Read,
            Mode::Collapsed,
        )));
        assert_eq!(header, "Read src/app/cli.rs (1-120)");

        let search = ToolItem {
            name: "search_files".into(),
            args: serde_json::json!({ "pattern": "todo", "path": "src" }),
            call_id: String::new(),
            output: Some(crate::transcript::ToolItemOutput {
                content: "src/a.rs:1:todo\nsrc/a.rs:9:todo\nsrc/b.rs:2:todo".into(),
                is_error: false,
            }),
            progress: String::new(),
        };
        let header = text(&Line::from(tool_header(
            &search,
            ToolKind::Search,
            Mode::Collapsed,
        )));
        // The pattern is Rust-debug-quoted, exactly as upstream prints it.
        assert_eq!(header, "Search \"todo\" in src (3 matches in 2 files)");
    }

    #[test]
    fn a_failed_tool_keeps_a_glyph_and_not_only_a_hue() {
        // The house rule that overrides fidelity: upstream turns the diamond
        // red and changes nothing else, which says nothing at 16 colours.
        // The glyphs come off the skin table, so pin the skin this file draws.
        let _pinned = crate::skin::pin(Skin::Grok);
        let (glyph, _) = tool_bullet(false, true);
        assert_ne!(glyph, DIAMOND);
        assert_eq!(tool_bullet(false, false).0, DIAMOND);
        assert_eq!(tool_bullet(true, false).0, DIAMOND);
    }

    #[test]
    fn a_commands_output_is_windowed_with_a_count_and_a_files_is_not() {
        let command = ToolItem {
            name: "execute".into(),
            args: serde_json::json!({ "command": "cargo test" }),
            call_id: String::new(),
            output: Some(crate::transcript::ToolItemOutput {
                content: (1..=12)
                    .map(|n| format!("line {n}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
                is_error: false,
            }),
            progress: String::new(),
        };
        let rows = tool_output(&command, ToolKind::Execute, false, 60);
        let joined: Vec<String> = rows.iter().map(|row| text(&row.line)).collect();
        assert_eq!(joined.len(), EXECUTE_FIRST + 1 + EXECUTE_LAST, "{joined:?}");
        assert_eq!(joined[EXECUTE_FIRST], "\u{2026} +7 lines");
        assert!(rows.iter().all(|row| row.panel), "output sits on a band");

        let file = ToolItem {
            name: "read_file".into(),
            args: serde_json::json!({ "path": "a.rs" }),
            call_id: String::new(),
            output: Some(crate::transcript::ToolItemOutput {
                content: (1..=12)
                    .map(|n| format!("line {n}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
                is_error: false,
            }),
            progress: String::new(),
        };
        let rows = tool_output(&file, ToolKind::Read, false, 60);
        let joined: Vec<String> = rows.iter().map(|row| text(&row.line)).collect();
        // A bare `…` with no count: the line-number gutter says how much is gone.
        assert_eq!(joined[READ_FIRST], "\u{2026}", "{joined:?}");
        // The gutter is right-aligned to the width of the largest number.
        assert!(joined[0].starts_with(" 1  line 1"), "{joined:?}");
        assert!(
            joined[READ_FIRST + 1].starts_with("10  line 10"),
            "{joined:?}"
        );
        assert!(!rows[0].panel, "a file read has no panel band");
    }

    // ── the screen ──────────────────────────────────────────────────────────

    #[test]
    fn the_frame_keeps_grok_builds_margins_and_puts_the_scrollbar_last() {
        let mut app = app();
        app.transcript.user("hello".to_string(), Vec::new());
        let layout = screen(&app, Rect::new(0, 0, 80, 24), 0, 3);
        // Two columns of outer margin either side: the block runs 2..=77, and
        // the scrollbar shares the right margin at column 79.
        assert_eq!(layout.body.x, 2);
        assert_eq!(layout.body.right(), 78);
        assert_eq!(layout.body.width, 76);
        // Content inside that: rail, two pads, 71 columns of prose, two pads.
        assert_eq!(content_width(layout.body.width), 71);
        assert_eq!(layout.composer.x, 2);
        assert_eq!(layout.composer.height, 3);
        // A shortcuts bar at the bottom, above one row of outer margin.
        assert_eq!(layout.shortcuts.bottom(), 23);
    }

    #[test]
    fn a_turn_in_flight_gets_a_status_row_and_an_idle_one_does_not() {
        let mut app = app();
        assert!(turn_status(&app).is_none(), "idle says nothing");
        app.status.busy = true;
        let line = turn_status(&app)
            .expect("a running turn narrates itself")
            .line(76);
        let rendered = text(&line);
        assert!(rendered.contains("[stop]"), "{rendered}");
        assert!(rendered.contains("step 0"), "{rendered}");
        // Upstream's fixed wording, not a shuffled verb pool — and Wizard's
        // own step counter, which no skin gets to withhold.
        assert!(rendered.contains("Thinking\u{2026}"), "{rendered}");
    }

    #[test]
    fn the_status_row_freezes_into_a_diamond_when_the_agent_is_blocked_on_you() {
        let mut app = app();
        app.status.busy = true;
        let (gate, _rx) = crate::agent::PlanGate::open();
        app.handle_agent_event(AgentEvent::PlanReady {
            plan: "do the thing".to_string(),
            gate,
        });
        let line = turn_status(&app)
            .expect("a blocked turn still narrates")
            .line(76);
        let rendered = text(&line);
        assert!(rendered.starts_with(DIAMOND), "{rendered}");
        assert!(rendered.contains("plan review"), "{rendered}");
        assert!(
            !rendered.contains("[stop]"),
            "no cancel while parked: {rendered}"
        );
    }

    #[test]
    fn an_idle_session_with_background_work_still_says_so() {
        let mut app = app();
        app.status.background_tasks = 1;
        app.status.background_subagents = 2;
        let rendered = text(
            &turn_status(&app)
                .expect("background work is narrated")
                .line(76),
        );
        assert!(
            rendered.contains("1 command \u{00b7} 2 subagents still running"),
            "{rendered}"
        );
    }

    #[test]
    fn wizards_own_state_survives_wearing_someone_elses_chrome() {
        let mut app = app();
        app.status.busy = true;
        app.status.background_subagents = 1;
        app.status.context_tokens = 8_500;
        app.plan_mode = true;
        app.handle_agent_event(AgentEvent::SubagentRunStarted {
            run: 1,
            bg: Some(1),
            name: "researcher".to_string(),
            task: "map the auth flow".to_string(),
        });
        let screen = render(&app, 100, 30).join("\n");
        assert!(screen.contains("plan"), "the mode is on the composer");
        assert!(screen.contains("genie"), "and so is the mode word");
        assert!(screen.contains("sub"), "background subagents are chipped");
        // The subagent survives, in the tasks pane's own grammar: a `▾
        // Subagents 1` header over a capitalized persona row.
        assert!(screen.contains("Subagents 1"), "the tasks pane groups them");
        assert!(screen.contains("Researcher"), "and names the run");
    }

    #[test]
    fn a_prompt_and_a_reply_render_in_their_own_columns() {
        let mut app = app();
        app.transcript
            .user("add a --json flag".to_string(), Vec::new());
        app.transcript.assistant("Sure, I'll do that.".to_string());
        app.transcript.commit();
        let rows = render(&app, 80, 24);
        let prompt = rows
            .iter()
            .find(|row| row.contains("add a --json flag"))
            .expect("the prompt is on screen");
        // Two columns of outer margin, one of (cleared) rail, two of pad, then
        // the `❯ `.
        assert!(
            prompt.starts_with("     \u{276f} add a --json flag"),
            "{prompt:?}"
        );
        let reply = rows
            .iter()
            .find(|row| row.contains("Sure"))
            .expect("the reply is on screen");
        // Same column, no marker: an agent message has no rail and no bullet.
        assert!(reply.starts_with("     Sure"), "{reply:?}");
    }

    #[test]
    fn the_welcome_screen_swaps_shape_at_ninety_columns() {
        let mut app = App::new(Config::default());
        app.status.model = "wizard-1".to_string();
        // The hero box is the only bordered thing on the whole screen.
        let wide = render(&app, 100, 30).join("\n");
        assert!(wide.contains('\u{256d}'), "a rounded box at 100 columns");
        assert!(wide.contains("Wizard"), "{wide}");
        // Below 90 it stacks, and the box is gone (bar the composer's own).
        let narrow = render(&app, 70, 30);
        let boxed = narrow.iter().filter(|row| row.contains('\u{256d}')).count();
        assert_eq!(boxed, 1, "only the composer is boxed below 90 columns");

        // Under both, the menu is a column: labels flush left, keys flush right
        // at a common edge. Centring the rows one at a time — which is what
        // this did before the rows were padded to `MENU_WIDTH` — puts every
        // key in a different place and the menu stops reading as one.
        for rows in [render(&app, 100, 30), narrow] {
            // Flush *right*, so what lines up is where each key ends. Counted
            // in cells, not bytes: the hero box carries the three-byte braille
            // mark to the left of the menu.
            let key_end = |needle: &str| {
                rows.iter().find(|row| row.contains(needle)).map(|row| {
                    let at = row.find(needle).expect("just matched");
                    row[..at].chars().count() + needle.chars().count()
                })
            };
            assert_eq!(
                key_end("/model"),
                key_end("/help"),
                "the keys share a right edge:\n{}",
                rows.join("\n")
            );
        }
    }

    #[test]
    fn a_tiny_terminal_still_gets_a_composer() {
        // Everything degrades before the composer does: the margins and the
        // gap rows are the first thing spent.
        let rows = render(&app(), 40, 8);
        assert!(
            rows.iter().any(|row| row.contains('\u{276f}')),
            "the prompt survives:\n{}",
            rows.join("\n")
        );
    }

    #[test]
    fn the_animation_speeds_are_upstreams_rescaled_to_wizards_clock() {
        // Upstream animates at 30fps; Wizard ticks at 10Hz. The wall-clock
        // cadence is what a person sees, so the speeds are tripled, not copied.
        assert!((WAVE_SPEED - 0.45).abs() < 1e-6);
        assert!((PULSE_SPEED - 0.24).abs() < 1e-6);
        // And a wave still stays inside the unit interval at every row.
        for tick in 0..40u64 {
            for row in 0..8u16 {
                let value = motion::wave(tick, row, WAVE_ROWS, WAVE_SPEED);
                assert!((0.0..=1.0).contains(&value));
            }
        }
    }
}
