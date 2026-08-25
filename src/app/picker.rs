//! Popup pickers, command suggestions, the status bar, and mouse selection.

use crate::commands::{CustomCommand, Listing};
use crate::config::{Mode, StepBudget};

/// One row in the suggestion popup: a [`Listing`] — built-in or plugin, the
/// popup does not care which — or a custom command loaded from
/// `~/.wizard/commands/` / `<project>/.wizard/commands/`.
#[derive(Debug, Clone)]
pub struct Suggestion {
    pub name: String,
    /// Argument hint shown after the name.
    pub args: String,
    pub description: String,
    /// Completion appends a trailing space and waits for arguments instead
    /// of submitting immediately.
    pub takes_args: bool,
}

impl From<&Listing> for Suggestion {
    fn from(row: &Listing) -> Self {
        Self {
            name: row.name.clone(),
            args: row.args.clone(),
            description: row.description.clone(),
            takes_args: row.takes_args,
        }
    }
}

impl From<&CustomCommand> for Suggestion {
    fn from(command: &CustomCommand) -> Self {
        let takes_args = command.expects_args();
        Self {
            name: command.name.clone(),
            args: if takes_args {
                "[args]".to_string()
            } else {
                String::new()
            },
            description: command
                .description
                .clone()
                .unwrap_or_else(|| "custom command".to_string()),
            takes_args,
        }
    }
}

/// True when `name` is a command word this build knows: a builtin table row,
/// one of the parse aliases with no row of its own, or a plugin registration.
/// Unknown words fall through to the model as a normal prompt.
///
/// Plugin commands are included because the question this answers is "does a
/// bad argument here deserve a usage notice, or is it just a prompt starting
/// with a slash", and a registered `/name` deserves the notice exactly as much
/// as `/rewind` does.
pub(super) fn is_builtin_command(name: &str) -> bool {
    crate::commands::is_known(name)
}

/// What an open [`Picker`] selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerKind {
    Model,
    Mode,
    /// Reasoning-effort level (item values are `low`/`medium`/`high`/`default`).
    Effort,
    /// A turn to rewind to (item values are turn ids).
    Rewind,
    /// A past session to resume (item values are session ids).
    Resume,
    /// A Claude Code conversation to import and continue (item values are
    /// Claude Code session ids). Opening one writes a *new* Wizard session and
    /// resumes that; `~/.claude` is only read. See
    /// [`crate::claude_resume`].
    ResumeClaude,
    /// A subagent to delegate to (item values are subagent names). Selecting
    /// one pre-fills the input with a delegation request rather than running a
    /// command, since subagents are invoked by the model, not directly.
    Subagent,
    /// The settings menu. Rows are dispatched by index against
    /// [`App::settings_rows`](super::App::settings_rows); toggles mutate config inline and re-open.
    Settings,
    /// "Import from Claude Code": a multi-select where Space toggles a row
    /// (MCP servers / commands / spinner verbs) and Enter runs the import.
    ClaudeImport,
    /// Level 1 of `/provider`: configured providers (Enter switches) plus a
    /// final "add provider" row that opens [`PickerKind::ProviderType`].
    Provider,
    /// Level 2 of `/provider`: the menu of provider kinds to add. Rows are
    /// dispatched by index against [`PROVIDER_TYPES`](super::prompts::PROVIDER_TYPES).
    ProviderType,
    /// The `web_search` backend picker (from `/settings`). Item values are
    /// backend ids ([`WEB_BACKENDS`](super::prompts::WEB_BACKENDS)); selecting a keyed backend starts an
    /// inline API-key prompt, xAI reuses the OAuth session, DuckDuckGo applies
    /// immediately.
    WebBackend,
    /// `/fusion config`: a multi-select where Space toggles a provider into the
    /// fusion panel and Enter saves `[fusion]` (the first toggled row becomes
    /// the synthesizer). Reuses [`PickerItem::current`] as the checkbox.
    FusionPanel,
    /// `/ultra config`: a multi-select over the lens catalog, with a final
    /// [`ULTRA_JUDGE_ROW`](super::prompts::ULTRA_JUDGE_ROW) row for the compare phase. Space toggles, Enter
    /// saves `[ultra]`. The toggled lens rows *are* the candidate count — one
    /// lens, one candidate — so this one picker sets both knobs the user cares
    /// about.
    UltraLenses,
}

/// One selectable row in a picker popup.
#[derive(Debug)]
pub struct PickerItem {
    /// Value applied on selection (model tag / mode name).
    pub value: String,
    /// Secondary text shown dimmed next to the value.
    pub detail: String,
    /// Marks the currently active item.
    pub current: bool,
}

/// An interactive selection popup (↑/↓ to move, Enter to select, Esc to
/// cancel).
#[derive(Debug)]
pub struct Picker {
    pub kind: PickerKind,
    pub title: String,
    pub items: Vec<PickerItem>,
    pub selected: usize,
}

impl Picker {
    /// Footer hint shown along the modal's bottom border. The Claude-import
    /// picker is a multi-select (Space toggles, Enter runs), so it needs a
    /// different hint than the Enter-to-select pickers.
    pub fn footer_hint(&self) -> &'static str {
        match self.kind {
            PickerKind::ClaudeImport => " ↑↓ move · space toggles · enter runs · Esc cancel ",
            PickerKind::FusionPanel | PickerKind::UltraLenses => {
                " ↑↓ move · space toggles · enter saves · Esc cancel "
            }
            _ => " ↑↓ move · Enter select · Esc cancel ",
        }
    }
}

/// Status bar contents.
#[derive(Debug, Default)]
pub struct StatusLine {
    pub model: String,
    pub mode: Mode,
    /// Current step within the running turn (0 when idle).
    pub step: u32,
    /// The turn's step budget — unlimited unless `max_steps` is configured.
    pub max_steps: StepBudget,
    /// True while a turn is streaming.
    pub busy: bool,
    /// Session prompt-token total (from [`AgentEvent::Usage`](crate::agent::AgentEvent::Usage)). Used by
    /// `/cost` for lifetime session usage / estimated spend — *not* the
    /// status-bar context meter.
    pub prompt_tokens: u64,
    /// Session completion-token total.
    pub completion_tokens: u64,
    /// Tokens that will load into context on the next model call (last
    /// reported prompt size, or a post-compact / post-clear estimate).
    /// This is what the status bar displays.
    pub context_tokens: u64,
    /// Background tasks (`execute` with `run_in_background`) still running.
    pub background_tasks: usize,
    /// Backgrounded subagents (`spawn_subagent` with `background: true`)
    /// still running.
    pub background_subagents: usize,
}

/// A mouse text selection over the rendered screen. Coordinates are absolute
/// terminal cells. Because wizard captures the mouse (so the wheel scrolls the
/// transcript), the terminal's own click-drag-to-select is pre-empted — so the
/// app draws the highlight itself ([`crate::ui`]) and copies the covered cells
/// to the clipboard on release, down every route that applies at once
/// ([`crate::app::term::copy_to_clipboard`]) rather than stopping at the first
/// one that reports success.
#[derive(Debug, Clone, Copy)]
pub struct Selection {
    /// Cell where the drag began (mouse-down).
    pub anchor: (u16, u16),
    /// Cell under the cursor now: tracks the drag, frozen on release.
    pub head: (u16, u16),
    /// True while the button is held down.
    pub dragging: bool,
}

impl Selection {
    /// The endpoints in reading order: `(start, end)` such that `start`
    /// precedes `end` row-major (top-to-bottom, then left-to-right).
    pub fn ordered(&self) -> ((u16, u16), (u16, u16)) {
        // Compare by (row, column) so a point lower on screen always sorts last.
        let key = |(x, y): (u16, u16)| (y, x);
        if key(self.anchor) <= key(self.head) {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    /// A click that never dragged: anchor and head are the same cell, so there
    /// is nothing to highlight or copy.
    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }
}
