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
    /// Level 2 of `/provider`: the backends this build can add, from
    /// [`provider_types`](super::prompts::provider_types). Rows are the ones
    /// whose `kind` a plugin registered, and Enter dispatches on the row's own
    /// `ProviderSetup` rather than on its position, because a filtered menu
    /// has no fixed positions.
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
    /// `/plugins [terms]`: Pi gallery hits (item values are specs). Enter
    /// asks which harness gets it, or installs for Wizard when Pi isn't here.
    PiGallery,
    /// Level 2 of an install: which harness (item values are
    /// `<target> <spec>`).
    PiTarget,
    /// `/plugins list`: what is installed for Wizard (item values are
    /// plugin names). Enter asks where to remove it from.
    PiInstalled,
    /// Level 2 of a remove (item values are `<target> <name>`).
    PiRemove,
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
    /// Index into [`Self::visible_items`], not the raw `items` vec.
    pub selected: usize,
    /// Type-to-filter query. Only consulted when [`Self::filterable`].
    pub filter: String,
}

impl Picker {
    /// True when typing narrows the list. Model only: Mode/Effort/multi-select
    /// keep their existing key bindings unchanged.
    pub fn filterable(&self) -> bool {
        matches!(self.kind, PickerKind::Model)
    }

    /// Empty filter matches everything; otherwise case-insensitive substring
    /// on `value` or `detail`.
    pub fn item_matches(item: &PickerItem, filter: &str) -> bool {
        if filter.is_empty() {
            return true;
        }
        let needle = filter.to_ascii_lowercase();
        item.value.to_ascii_lowercase().contains(&needle)
            || item.detail.to_ascii_lowercase().contains(&needle)
    }

    /// Rows the user can see and navigate: all items when not filterable,
    /// otherwise those matching [`Self::filter`]. Yields `(original index,
    /// item)`.
    pub fn visible_items(&self) -> impl Iterator<Item = (usize, &PickerItem)> {
        let filterable = self.filterable();
        let filter = self.filter.as_str();
        self.items
            .iter()
            .enumerate()
            .filter(move |(_, item)| !filterable || Self::item_matches(item, filter))
    }

    /// Keep `selected` inside the visible list after the filter changes.
    pub fn clamp_selected(&mut self) {
        let len = self.visible_items().count();
        if len == 0 {
            self.selected = 0;
        } else if self.selected >= len {
            self.selected = len - 1;
        }
    }

    /// Footer hint shown along the modal's bottom border. The Claude-import
    /// picker is a multi-select (Space toggles, Enter runs), so it needs a
    /// different hint than the Enter-to-select pickers.
    pub fn footer_hint(&self) -> &'static str {
        match self.kind {
            PickerKind::Model => {
                " type to filter · ↑↓ move · enter select · esc cancel "
            }
            PickerKind::ClaudeImport => " ↑↓ move · space toggles · enter runs · esc cancel ",
            PickerKind::FusionPanel | PickerKind::UltraLenses => {
                " ↑↓ move · space toggles · enter saves · esc cancel "
            }
            _ => " ↑↓ move · enter select · esc cancel ",
        }
    }
}

/// Level 2 of a `/plugins` install: which harness gets `spec`. `None` when
/// Pi isn't on this machine, where the only answer is Wizard and asking would
/// be a one-row menu.
pub(super) fn plugin_target_picker(spec: &str, pi_available: bool) -> Option<Picker> {
    use crate::pi_plugins::Target;
    if !pi_available {
        return None;
    }
    let name = spec.strip_prefix("npm:").unwrap_or(spec);
    let row = |target: Target, detail: &str| PickerItem {
        value: format!("{} {spec}", target_word(target)),
        detail: format!("{} \u{b7} {detail}", target.label()),
        current: false,
    };
    Some(Picker {
        kind: PickerKind::PiTarget,
        title: format!(" install {name} for "),
        items: vec![
            row(Target::Both, "Wizard is beta: skills and prompts work"),
            row(
                Target::Wizard,
                "beta: skills and prompts work, extensions don't yet",
            ),
            row(Target::Pi, "runs pi install"),
        ],
        selected: 0,
        filter: String::new(),
    })
}

/// Level 2 of a `/plugins list` pick: where to remove `name` from.
pub(super) fn plugin_remove_picker(name: &str, pi_available: bool) -> Picker {
    use crate::pi_plugins::Target;
    let row = |target: Target, detail: &str| PickerItem {
        value: format!("{} {name}", target_word(target)),
        detail: detail.to_string(),
        current: false,
    };
    let mut items = vec![row(Target::Wizard, "remove from Wizard")];
    if pi_available {
        items.push(row(Target::Both, "remove from Pi and Wizard"));
    }
    Picker {
        kind: PickerKind::PiRemove,
        title: format!(" remove {name} "),
        items,
        selected: 0,
        filter: String::new(),
    }
}

fn target_word(target: crate::pi_plugins::Target) -> &'static str {
    use crate::pi_plugins::Target;
    match target {
        Target::Wizard => "wizard",
        Target::Pi => "pi",
        Target::Both => "both",
    }
}

/// `both npm:x` → `(Both, "npm:x")`: the value a target row carries.
pub(super) fn split_target(value: &str) -> Option<(crate::pi_plugins::Target, String)> {
    use crate::pi_plugins::Target;
    let (word, rest) = value.split_once(' ')?;
    let target = match word {
        "wizard" => Target::Wizard,
        "pi" => Target::Pi,
        "both" => Target::Both,
        _ => return None,
    };
    Some((target, rest.to_string()))
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
