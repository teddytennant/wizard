// OWNER: shared (traits and layout helpers; add, do not reshape)
//! The widgets of the chat screen and the contracts between them.
//!
//! Layout of the inline viewport, top to bottom (spec A.4.1): the in-flight cell (inset one row),
//! then the bottom pane (inset one row): status indicator, queued input preview, composer band,
//! footer. Everything above the viewport is scrollback, written once through
//! `term::history_insert`.

pub mod approval;
pub mod bottom_pane;
pub mod composer;
pub mod composer_history;
pub mod diff_preview;
pub mod diff_render;
pub mod exec_cell;
pub mod footer;
pub mod history_cells;
pub mod key_hint;
pub mod mention_popup;
pub mod pager_overlay;
pub mod paste_burst;
pub mod pickers;
pub mod session_header;
pub mod shell_cell;
pub mod skill_popup;
pub mod slash_popup;
pub mod status_indicator;
pub mod textarea;

use std::fmt::Debug;

use crossterm::event::KeyEvent;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::text::Line;

/// A finished unit of the conversation. Cells are the source of truth: scrollback is a render of
/// them at one width, and a resize re-renders them all (spec A.5).
pub trait HistoryCell: Debug + Send {
    /// Lines for the scrollback at `width`, without a trailing blank row. The separator between
    /// cells is added by the app.
    fn display_lines(&self, width: u16) -> Vec<Line<'static>>;
    /// Lines for the Ctrl+T pager. Exec cells differ from their inline form.
    fn transcript_lines(&self, width: u16) -> Vec<Line<'static>> {
        self.display_lines(width)
    }
    /// True for the second and later chunks of one streamed answer: no blank row before them.
    fn is_stream_continuation(&self) -> bool {
        false
    }
    /// Tinted user-message rows get a reversed style when the backtrack preview selects them.
    fn is_user_message(&self) -> bool {
        false
    }
    /// Copy-friendly lines for raw output mode (`/raw`): source text where the cell has one,
    /// the plain text of its rows otherwise, never wrapped.
    fn raw_lines(&self) -> Vec<Line<'static>> {
        plain_lines(self.display_lines(RAW_WIDTH))
    }
}

/// Width a cell is rendered at when its raw form is its rich text without styling. Wide enough
/// that nothing wraps; the terminal does the breaking.
pub const RAW_WIDTH: u16 = 2000;

/// The text of each line with every style removed (and the padding a tinted row ends in).
pub fn plain_lines(lines: impl IntoIterator<Item = Line<'static>>) -> Vec<Line<'static>> {
    lines
        .into_iter()
        .map(|l| {
            let text: String = l
                .spans
                .into_iter()
                .map(|s| s.content.into_owned())
                .collect();
            Line::from(text.trim_end().to_string())
        })
        .collect()
}

/// One line per source line; a trailing newline adds nothing, an explicit blank line stays.
pub fn raw_lines_from_source(source: &str) -> Vec<Line<'static>> {
    if source.is_empty() {
        return Vec::new();
    }
    let mut parts: Vec<&str> = source.split('\n').collect();
    if source.ends_with('\n') {
        parts.pop();
    }
    parts
        .into_iter()
        .map(|l| Line::from(l.to_string()))
        .collect()
}

pub type BoxedCell = Box<dyn HistoryCell>;

/// What a bottom-pane view or an overlay asks the app to do.
#[derive(Clone, Debug, PartialEq)]
pub enum AppAction {
    /// Pick a model by backend id (`provider/name`).
    SetModel(String),
    SetEffort(String),
    SetMode(String),
    /// The model picker's answer: either part may be absent (`/model <id>` sets only the model).
    ChangeModel {
        model: Option<String>,
        effort: Option<String>,
    },
    /// Name this session (`/rename`).
    RenameSession(String),
    /// The startup resume picker was dismissed: carry on with a new session.
    StartFresh,
    /// Answer an `Event::Permission`.
    Decide {
        id: String,
        allow: bool,
        always: bool,
        note: String,
    },
    /// Open a session picked in the resume picker.
    LoadSession(String),
    /// Run text as if it had been typed in the composer.
    Submit(String),
    /// Append an info cell.
    Info(String),
    /// Append an error cell.
    Error(String),
    OpenPager,
    /// A view's answer to an `Event::Permission`, with the cells to print.
    Decision(approval::Decision),
    /// Backtrack confirmed on the `nth` user message (0 based): put its text back in the composer.
    EditPrompt {
        nth: usize,
        text: String,
    },
    /// Open a static pager (`E X E C`, `P A T C H`) from a bottom view.
    ShowStatic {
        title: String,
        lines: Vec<Line<'static>>,
    },
    /// Confirmed rewind: send wizard `/rewind <turn>`, then load the cut session. `text` is the
    /// prompt that goes back into the composer.
    Rewind {
        turn: u64,
        text: String,
    },
    /// `/statusline` confirmed: the item ids in order and whether the line takes theme colours.
    StatusLineSet {
        items: Vec<String>,
        colors: bool,
    },
    /// `/title` confirmed: the item ids in order.
    TitleSet(Vec<String>),
    /// A syntax theme picked in `/theme`: use it and save it.
    SyntaxThemeSelected(String),
    /// Put text in the composer at the cursor, as if typed (`/skills`, `List skills`).
    InsertText(String),
    Quit,
}

/// What a key did inside a view.
#[derive(Clone, Debug, PartialEq)]
pub enum ViewResult {
    /// Key handled (or ignored), the view stays.
    Pending,
    /// Close the view.
    Close,
    /// Close the view and do this.
    CloseWith(AppAction),
    /// Do this and keep the view open.
    Action(AppAction),
}

/// A modal view that replaces the composer in the bottom pane (selection lists, approvals).
/// The composer keeps its state underneath.
pub trait BottomView: Debug + Send {
    fn desired_height(&self, width: u16) -> u16;
    fn render(&self, area: Rect, buf: &mut Buffer);
    fn handle_key(&mut self, key: KeyEvent) -> ViewResult;
    fn handle_paste(&mut self, _text: &str) {}
    /// True when the user must answer (approval): the terminal title says so.
    fn needs_action(&self) -> bool {
        false
    }
    fn cursor(&self, _area: Rect) -> Option<Position> {
        None
    }
}

/// What an alt-screen overlay (pager, `/diff`, resume picker) tells the app.
#[derive(Clone, Debug, PartialEq)]
pub enum OverlayResult {
    Pending,
    Close,
    CloseWith(AppAction),
}

/// A full-screen view. Rendered on the alternate screen, never inline.
pub trait Overlay: Debug + Send {
    fn render(&mut self, area: Rect, buf: &mut Buffer);
    fn handle_key(&mut self, key: KeyEvent) -> OverlayResult;
    /// A bracketed paste while the overlay is open.
    fn handle_paste(&mut self, _text: &str) {}
    /// Called before every draw with the committed cells and the in-flight cell's lines, so a
    /// pager can follow the conversation without owning the cells.
    fn sync_cells(&mut self, _cells: &[BoxedCell], _live_tail: &[Line<'static>], _width: u16) {}
    /// Redraw delay while something animates, `None` when static (idle CPU stays at zero).
    fn animation_interval(&self) -> Option<std::time::Duration> {
        None
    }
}

/// Display width of a string.
pub fn text_width(s: &str) -> usize {
    crate::wrap::width_of(s)
}

/// Write `line` into `buf` starting at `area`, clipped to its width. Returns columns used.
pub fn put_line(buf: &mut Buffer, area: Rect, y: u16, line: &Line<'_>) -> u16 {
    tuikit::paint::put_line(buf, area.x, y, line, area)
}
