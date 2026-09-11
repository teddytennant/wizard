//! UI skins: which coding agent's terminal chrome the TUI wears.
//!
//! [`crate::theme`] answers "what color is an accent". This module answers the
//! other half: what *shape* the chrome is. A skin owns the gutter markers, the
//! tool-card glyphs and label grammar, the composer frame, the welcome screen,
//! the spinner, and the wording of the status line. Four ship:
//!
//! - `wizard` — the house look: one rule over the composer, `❯`/`·` gutters,
//!   a status line of facts and nothing decorative. `docs/design.md` is its
//!   standard.
//! - `codex` — OpenAI Codex's: a `>_` banner, `›` for the user and `•` for the
//!   agent, `Ran <cmd>` with a `└` output arm, a bare composer under a `›`
//!   prompt, and `Working (12s • esc to interrupt)`.
//! - `grok` — xAI's Grok Build: a `┃` accent bar down the side of every block,
//!   colored by whose block it is, a bordered composer, and `Thinking… 12s`.
//!
//! **What a skin is not.** It does not change what Wizard *is*. The commands
//! are Wizard's (`/model`, `/fusion`, `/ultra`, `/publish`), onboarding is
//! Wizard's, the status line reports Wizard's own state — mode, ultra
//! multiplier, background subagents, the context meter — under whichever skin
//! is on. Wearing Codex's chrome must never imply Codex's feature set, so the
//! skin layer is deliberately given no way to add, hide, or rename a command:
//! it maps state that already exists onto a different set of glyphs. What it
//! borrows is a *look*, and it says whose look it is on the welcome screen.
//!
//! **Colors still come from the theme.** Nothing here names a
//! [`ratatui::style::Color`] — a skin names a [`Token`], exactly like
//! `src/ui/mod.rs` does, which is what keeps the low-color fallback and `NO_COLOR`
//! working under all four. What a skin *may* do is name the theme it looks
//! best in ([`Skin::companion_theme`]); that choice sits at the bottom of the
//! theme resolution order, under both `[ui] theme` and `WIZARD_THEME`, so
//! picking a skin never overrides a palette the user chose on purpose.
//!
//! Resolution order, highest first: `[ui] skin` in `config.toml`
//! ([`crate::config::UiConfig::skin`]), then the `WIZARD_SKIN` environment
//! variable, then [`DEFAULT_SKIN`]. See [`resolve_name`], which mirrors
//! [`crate::theme::resolve_name`] line for line — two settings that resolve by
//! different rules would be a trap, not a feature.

use std::cell::RefCell;
use std::sync::{OnceLock, PoisonError, RwLock};

use anyhow::{Result, bail};

use crate::theme::Token;

pub mod blend;
pub mod layout;
pub mod motion;

pub use blend::Tint;
pub use layout::{Accent, BlockKind, BlockStyle, Marker};

/// Columns a skin that hangs its prompt in the margin reserves for it, and the
/// indent everything aligned to that prompt uses — Codex's `LIVE_PREFIX_COLS`.
pub const LIVE_PREFIX_COLS: usize = 2;

/// Skin used when nothing else is chosen.
pub const DEFAULT_SKIN: &str = "wizard";

/// Environment variable naming the skin, below `[ui] skin` in `config.toml`
/// ([`crate::config::UiConfig::skin`]) and above [`DEFAULT_SKIN`].
pub const ENV_SKIN: &str = "WIZARD_SKIN";

// ---------------------------------------------------------------------------
// The skins
// ---------------------------------------------------------------------------

/// Which agent's terminal chrome the TUI wears.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Skin {
    /// The house look (default).
    #[default]
    Wizard,
    /// OpenAI Codex's chrome.
    Codex,
    /// xAI Grok Build's chrome.
    Grok,
}

impl Skin {
    /// Every skin, in the order pickers and listings show them.
    pub const ALL: [Skin; 3] = [Skin::Wizard, Skin::Codex, Skin::Grok];

    /// The key written to `[ui] skin` and accepted by `/ui <name>`.
    pub fn key(self) -> &'static str {
        match self {
            Skin::Wizard => "wizard",
            Skin::Codex => "codex",
            Skin::Grok => "grok",
        }
    }

    /// How the skin names itself on screen.
    pub fn label(self) -> &'static str {
        match self {
            Skin::Wizard => "wizard",
            Skin::Codex => "codex",
            Skin::Grok => "grok build",
        }
    }

    /// One line for `/ui` and the onboarding picker.
    pub fn description(self) -> &'static str {
        match self {
            Skin::Wizard => "the house look: one rule, no boxes, nothing decorative (default)",
            Skin::Codex => "OpenAI Codex: >_ banner, › prompts, • bullets, └ output",
            Skin::Grok => "Grok Build: a colored ┃ bar down every block, boxed composer",
        }
    }

    /// Parse a skin name. Generous about spelling because the names people
    /// reach for are the product names, not the keys: "grok" and "grok build"
    /// are the same request, and so are "codex" and "openai codex".
    pub fn from_key(key: &str) -> Option<Skin> {
        let key = key.trim().to_ascii_lowercase();
        match key.replace([' ', '_'], "-").as_str() {
            "wizard" | "default" | "house" => Some(Skin::Wizard),
            "codex" | "openai" | "openai-codex" => Some(Skin::Codex),
            "grok" | "grok-build" | "grokbuild" | "xai" => Some(Skin::Grok),
            _ => None,
        }
    }

    /// The theme this skin looks best in, used *only* when the user has chosen
    /// no theme at all (see [`crate::theme::resolve_name`], which takes this as
    /// its bottom-of-the-order fallback).
    ///
    /// Each borrowed look has a palette as recognizable as its layout —
    /// Codex's cyan, Grok Build's violet on gray — and shipping
    /// the chrome without the color would be an impression of the thing rather
    /// than the thing. But a palette is also the setting people are most
    /// likely to have already made deliberately, so this loses to `[ui] theme`
    /// and to `WIZARD_THEME` both.
    pub fn companion_theme(self) -> &'static str {
        match self {
            Skin::Wizard => crate::theme::DEFAULT_THEME,
            Skin::Codex => "codex",
            Skin::Grok => "grok",
        }
    }

    /// The chrome this skin draws with.
    pub fn chrome(self) -> &'static Chrome {
        match self {
            Skin::Wizard => &WIZARD,
            Skin::Codex => &CODEX,
            Skin::Grok => &GROK,
        }
    }
}

// ---------------------------------------------------------------------------
// Chrome
// ---------------------------------------------------------------------------

/// The per-kind block styles one skin declares.
///
/// A [`BlockStyle`] carries everything about how an entry occupies the screen:
/// the accent column, the pads, the vertical padding, the slab behind it, and
/// the marker that leads its content. See [`crate::skin::layout`], which owns
/// the model and the reasons for it.
#[derive(Debug, Clone, Copy)]
pub struct Blocks {
    pub user: BlockStyle,
    pub assistant: BlockStyle,
    pub thinking: BlockStyle,
    pub tool: BlockStyle,
    pub notice: BlockStyle,
}

impl Blocks {
    /// The style for one kind of block.
    pub fn of(&self, kind: BlockKind) -> &BlockStyle {
        match kind {
            BlockKind::User => &self.user,
            BlockKind::Assistant => &self.assistant,
            BlockKind::Thinking => &self.thinking,
            BlockKind::Tool => &self.tool,
            BlockKind::Notice => &self.notice,
        }
    }
}

/// A block with a marker and nothing else: no column, no pads, no slab. What
/// `wizard` uses for everything.
const fn plain(marker: Marker) -> BlockStyle {
    BlockStyle::plain(marker)
}

/// A block in Grok Build's column geometry: one column of rail, two of left
/// padding, two held back at the right. Five columns of chrome in total —
/// upstream's `block_pad_right` is 2, whatever its own stale doc comment and
/// `CHROME_WIDTH` constant say.
const fn barred(
    glyph: &'static str,
    token: Token,
    tint: Option<Tint>,
    animate: bool,
) -> BlockStyle {
    BlockStyle {
        accent: Some(Accent {
            glyph,
            token,
            animate,
        }),
        pad_left: 2,
        pad_right: 2,
        pad_y: 0,
        tint,
        gap_before: 1,
        marker: Marker::none(),
    }
}

/// The rail glyph. A blank one still *reserves* the column, which is what an
/// agent message gets: Grok Build gives it no rail at all, and clearing the
/// column rather than dropping it is what keeps its prose in the same column
/// as everything else.
const RAIL: &str = "\u{2503}";
const NO_RAIL: &str = " ";

/// How a tool call's header reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolLabel {
    /// `bash  ls -la` — the name, then its arguments, dim.
    Plain,
    /// `Ran ls -la` — Codex's past-tense narration.
    Ran,
}

/// How the composer is framed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposerFrame {
    /// A dim rule above and below, no box.
    Rules,
    /// A box in the theme's border style.
    Boxed,
    /// Nothing: a blank row above and below, so the prompt glyph floats.
    Bare,
}

/// What fills the screen before the first message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WelcomeStyle {
    /// The braille wand-and-spark mark over a centered card.
    Mark,
    /// A `>_` banner over a left-aligned key/value block.
    Banner,
    /// A left-aligned block behind a full-height accent bar.
    Bar,
}

/// How the status line narrates a turn in flight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BusyStyle {
    /// `step 3 · 12s` — what Wizard is doing, counted.
    Steps,
    /// `Working (12s • esc to interrupt)`.
    Working,
    /// `Thinking… 12s`.
    Thinking,
}

/// Everything the renderer asks the skin for. All of it is `&'static`: a skin
/// is a table, not a trait object, so switching one at runtime is a pointer
/// swap and nothing allocates per frame.
#[derive(Debug, Clone, Copy)]
pub struct Chrome {
    /// How each kind of transcript block occupies the screen.
    pub blocks: Blocks,
    /// What a tool card shows once it has finished cleanly.
    pub tool_done: &'static str,
    /// What a tool card shows once it has failed.
    pub tool_failed: &'static str,
    /// Grammar of the tool-card header.
    pub tool_label: ToolLabel,
    /// The arm a tool's output hangs off, inside its block: the first row's
    /// prefix, then every row below it. Both must be the same display width,
    /// or the body goes ragged.
    pub tool_output: (&'static str, &'static str),
    /// Spinner frames for a running tool and a busy status line.
    pub spinner: &'static [char],
    /// The composer's prompt glyph. Two display columns.
    pub prompt: &'static str,
    /// How the composer is framed.
    pub composer: ComposerFrame,
    /// What fills the screen before the first message.
    pub welcome: WelcomeStyle,
    /// What sits between two chips on the status line.
    pub separator: &'static str,
    /// How a turn in flight is narrated.
    pub busy: BusyStyle,
    /// Right-aligned hint when nothing else is going on.
    pub idle_hint: &'static str,
    /// Does a running turn get its own row *above* the composer (Codex), or
    /// does it share the status bar below everything (Wizard, Grok Build)?
    /// Two genuinely different screen layouts, which is why this is a layout
    /// question and not a wording one.
    pub status_above: bool,
}

/// The house look. `docs/design.md` says why each of these is what it is.
const WIZARD: Chrome = Chrome {
    blocks: Blocks {
        user: plain(Marker::hanging("❯ ", Token::Faint, true)),
        assistant: plain(Marker::hanging("· ", Token::Accent, false)),
        thinking: plain(Marker::hanging("· ", Token::Faint, false)),
        tool: plain(Marker::none()),
        notice: plain(Marker::hanging("  ", Token::Faint, false)),
    },
    tool_done: "✓",
    tool_failed: "✗",
    tool_label: ToolLabel::Plain,
    tool_output: ("  ", "  "),
    spinner: &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'],
    prompt: "❯ ",
    composer: ComposerFrame::Rules,
    welcome: WelcomeStyle::Mark,
    separator: " · ",
    busy: BusyStyle::Steps,
    // Nothing when idle: a hint nobody asked for is chrome, and `/` is one
    // keystroke away from listing every command.
    idle_hint: "",
    status_above: false,
};

/// OpenAI Codex's chrome.
///
/// `›` for the user, `•` for everything the agent says, `Ran <cmd>` headers
/// with a `└` arm under them, and a composer with no frame at all — Codex
/// hangs its prompt glyph in the margin and lets the draft sit on the
/// terminal's own background.
const CODEX: Chrome = Chrome {
    blocks: Blocks {
        // The one block Codex gives a slab and a margin to. `pad_y: 1` is the
        // blank styled row it pushes above and below the message, which is
        // what turns the tint into a panel instead of a highlighted paragraph.
        user: BlockStyle {
            pad_y: 1,
            pad_right: 1,
            tint: Some(Tint::Raised),
            // `"› ".bold().dim()` upstream: both modifiers on one span, which
            // is why this is the faint token *and* bold rather than either.
            ..plain(Marker::hanging("› ", Token::Faint, true))
        },
        assistant: plain(Marker::hanging("• ", Token::Faint, false)),
        thinking: plain(Marker::hanging("• ", Token::Faint, false)),
        tool: plain(Marker::none()),
        notice: plain(Marker::hanging("• ", Token::Faint, false)),
    },
    tool_done: "•",
    tool_failed: "•",
    tool_label: ToolLabel::Ran,
    tool_output: ("  └ ", "    "),
    spinner: &['·', '✢', '✳', '∗', '✳', '✢'],
    prompt: "› ",
    composer: ComposerFrame::Bare,
    welcome: WelcomeStyle::Banner,
    separator: "  ",
    busy: BusyStyle::Working,
    idle_hint: "? for shortcuts",
    status_above: true,
};

/// xAI Grok Build's chrome.
///
/// One idea, applied everywhere: a `┃` bar down the left of every block,
/// colored by whose block it is — the user's quiet, the agent's accented, a
/// tool's faint and breathing while it runs. The column geometry is Grok
/// Build's own (`scrollback/layout.rs`): one column of bar, two of padding,
/// one held back at the right.
///
/// The slabs behind the user and tool blocks are blended against the
/// terminal's own background rather than declared as colors, so they lift off
/// a dark theme and settle onto a light one — and vanish entirely when the
/// background cannot be known, which is the honest answer rather than a guess.
/// See [`crate::skin::blend`].
const GROK: Chrome = Chrome {
    blocks: Blocks {
        // The prompt band, and the *only* block upstream gives either vertical
        // padding or a background to: everything else overrides both away.
        user: BlockStyle {
            pad_y: 1,
            ..barred(RAIL, Token::Muted, Some(Tint::Raised), false)
        },
        // No rail on an agent message — the column is reserved and left
        // blank, so the prose lines up with the blocks that do have one.
        assistant: barred(NO_RAIL, Token::Accent, None, false),
        // Reasoning is the one that animates both its rail and its bullet.
        thinking: barred(RAIL, Token::Faint, None, true),
        tool: barred(RAIL, Token::Faint, None, true),
        notice: barred(RAIL, Token::Muted, None, false),
    },
    // Upstream carries tool status in *color alone*, on the rail and on a `◆`
    // bullet. Wizard keeps a distinct glyph for failure and only for failure:
    // the house rule that meaning never rests on hue is what keeps the TUI
    // readable at 16 colors and under `NO_COLOR`, and "the tool failed" is the
    // one state where losing that would actually cost the user something.
    tool_done: "◆",
    tool_failed: "✗",
    tool_label: ToolLabel::Ran,
    tool_output: ("  ", "  "),
    spinner: &['◐', '◓', '◑', '◒'],
    prompt: "❯ ",
    composer: ComposerFrame::Boxed,
    welcome: WelcomeStyle::Bar,
    separator: " · ",
    busy: BusyStyle::Thinking,
    idle_hint: "/ commands · ctrl+c quit",
    status_above: false,
};

// ---------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------

/// Every skin key, for listings and error messages.
pub fn available() -> Vec<&'static str> {
    Skin::ALL.iter().map(|skin| skin.key()).collect()
}

/// Resolution order: `[ui] skin` from `config.toml`
/// ([`crate::config::UiConfig::skin`]), then `WIZARD_SKIN`, then
/// [`DEFAULT_SKIN`]. Blank values at either level are treated as absent, so
/// neither `WIZARD_SKIN=` nor a `skin = ""` someone cleared selects a skin
/// called "": both fall through, which is what blanking a setting means.
///
/// An *unknown* name is not the same as a blank one and does not fall through
/// here — [`init`] loads it, fails, and reports the typo, because silently
/// running the default is how a misspelled setting goes unnoticed for weeks.
pub fn resolve_name(config: Option<&str>, env: Option<&str>) -> String {
    let pick = |value: Option<&str>| {
        value
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    pick(config)
        .or_else(|| pick(env))
        .unwrap_or_else(|| DEFAULT_SKIN.to_string())
}

/// Load a skin by name.
pub fn load(name: &str) -> Result<Skin> {
    match Skin::from_key(name) {
        Some(skin) => Ok(skin),
        None => bail!(
            "unknown ui '{name}' (available: {})",
            available().join(", ")
        ),
    }
}

/// Install the active skin at process start.
///
/// `config_choice` is [`crate::config::UiConfig::skin`]; passing it is what
/// makes this function's half of the resolution order real. Returns a warning
/// to show the user when the named skin does not exist — the default is
/// installed in that case, because a typo in a config file must not cost
/// anyone their TUI.
///
/// Installs through [`set_global`] and not [`set_active`], for the reason
/// [`crate::theme::init`] documents at length: `App::new` calls this on every
/// construction, and writing through a thread's pin would let a freshly built
/// `App` replace the skin another renderer had pinned for itself.
pub fn init(config_choice: Option<&str>) -> Option<String> {
    let env = std::env::var(ENV_SKIN).ok();
    let (skin, warning) = init_skin(config_choice, env.as_deref());
    set_global(skin);
    warning
}

/// Testable core of [`init`]: resolve and load, install nothing. Separated for
/// the same reason `theme::init_theme` is — the whole suite writes the
/// process-wide slot, so a test that called `init` and then read [`active`]
/// would be asserting against a value other threads are writing.
fn init_skin(config_choice: Option<&str>, env: Option<&str>) -> (Skin, Option<String>) {
    let name = resolve_name(config_choice, env);
    match load(&name) {
        Ok(skin) => (skin, None),
        Err(err) => (
            Skin::default(),
            Some(format!("ui: {err:#}; using {DEFAULT_SKIN}")),
        ),
    }
}

// ---------------------------------------------------------------------------
// The active skin
// ---------------------------------------------------------------------------

fn global() -> &'static RwLock<Skin> {
    static ACTIVE: OnceLock<RwLock<Skin>> = OnceLock::new();
    ACTIVE.get_or_init(|| RwLock::new(Skin::default()))
}

thread_local! {
    /// A skin pinned to this thread, which wins over the process-wide one.
    /// Tests use it to render a known chrome without disturbing any other
    /// thread.
    static PINNED: RefCell<Option<Skin>> = const { RefCell::new(None) };
}

fn set_global(skin: Skin) {
    *global().write().unwrap_or_else(PoisonError::into_inner) = skin;
}

/// The skin in force on this thread.
pub fn active() -> Skin {
    if let Some(skin) = PINNED.with(|pinned| *pinned.borrow()) {
        return skin;
    }
    *global().read().unwrap_or_else(PoisonError::into_inner)
}

/// The chrome in force on this thread. What `src/ui/mod.rs` calls, once per thing
/// it draws.
pub fn chrome() -> &'static Chrome {
    active().chrome()
}

/// Swap the active skin. A thread that has pinned one keeps its pin (the swap
/// lands there); otherwise this changes the skin process-wide.
pub fn set_active(skin: Skin) {
    let pinned = PINNED.with(|pinned| {
        let mut slot = pinned.borrow_mut();
        if slot.is_some() {
            *slot = Some(skin);
            true
        } else {
            false
        }
    });
    if !pinned {
        set_global(skin);
    }
}

/// Swap to a named skin.
pub fn set_active_by_name(name: &str) -> Result<Skin> {
    let skin = load(name)?;
    set_active(skin);
    Ok(skin)
}

/// Pin `skin` to the current thread until the returned guard drops.
pub fn pin(skin: Skin) -> Pinned {
    let previous = PINNED.with(|pinned| pinned.borrow_mut().replace(skin));
    Pinned { previous }
}

/// Guard returned by [`pin`]; restores the previous pin on drop.
pub struct Pinned {
    previous: Option<Skin>,
}

impl Drop for Pinned {
    fn drop(&mut self) {
        let previous = self.previous.take();
        PINNED.with(|pinned| *pinned.borrow_mut() = previous);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_skin_round_trips_through_its_key() {
        for skin in Skin::ALL {
            assert_eq!(Skin::from_key(skin.key()), Some(skin), "{}", skin.key());
        }
    }

    #[test]
    fn product_names_and_spellings_resolve() {
        for (spelling, expected) in [
            ("OpenAI Codex", Skin::Codex),
            ("grok build", Skin::Grok),
            ("grok_build", Skin::Grok),
            ("default", Skin::Wizard),
        ] {
            assert_eq!(Skin::from_key(spelling), Some(expected), "{spelling}");
        }
        assert_eq!(Skin::from_key("gemini"), None);
    }

    #[test]
    fn resolution_order_is_config_then_env_then_default() {
        assert_eq!(resolve_name(Some("codex"), Some("grok")), "codex");
        assert_eq!(resolve_name(None, Some("grok")), "grok");
        assert_eq!(resolve_name(None, None), DEFAULT_SKIN);
        // Blank at either level means "unset", not a skin named "".
        assert_eq!(resolve_name(Some("  "), Some("grok")), "grok");
        assert_eq!(resolve_name(Some(""), None), DEFAULT_SKIN);
        assert_eq!(resolve_name(None, Some("")), DEFAULT_SKIN);
    }

    #[test]
    fn an_unknown_name_warns_and_falls_back_rather_than_failing() {
        let (skin, warning) = init_skin(Some("emacs"), None);
        assert_eq!(skin, Skin::Wizard);
        let warning = warning.expect("a typo must be reported");
        assert!(warning.contains("emacs"), "{warning}");
        assert!(warning.contains(DEFAULT_SKIN), "{warning}");
    }

    #[test]
    fn every_markers_two_rows_are_the_same_width() {
        // A marker whose head and continuation disagree makes the block go
        // ragged at every wrap, which is the failure the whole wrap-then-
        // decorate order exists to avoid.
        use unicode_width::UnicodeWidthStr;
        for skin in Skin::ALL {
            let chrome = skin.chrome();
            for kind in [
                BlockKind::User,
                BlockKind::Assistant,
                BlockKind::Thinking,
                BlockKind::Tool,
                BlockKind::Notice,
            ] {
                let marker = chrome.blocks.of(kind).marker;
                assert_eq!(
                    marker.head.width(),
                    marker.rest.width(),
                    "{} {kind:?} marker head and rest disagree",
                    skin.key()
                );
            }
            assert_eq!(chrome.prompt.width(), 2, "{} prompt", skin.key());
            let (first, rest) = chrome.tool_output;
            assert_eq!(
                first.width(),
                rest.width(),
                "{} tool output arm and its continuation must align",
                skin.key()
            );
        }
    }

    #[test]
    fn an_accent_column_is_exactly_one_column() {
        // The block layout reserves one cell for it (Grok Build's geometry);
        // a two-column glyph would push every content row one cell right of
        // where the width math put it.
        use unicode_width::UnicodeWidthStr;
        for skin in Skin::ALL {
            for kind in [
                BlockKind::User,
                BlockKind::Assistant,
                BlockKind::Thinking,
                BlockKind::Tool,
                BlockKind::Notice,
            ] {
                if let Some(accent) = skin.chrome().blocks.of(kind).accent {
                    assert_eq!(accent.glyph.width(), 1, "{} {kind:?}", skin.key());
                }
            }
        }
    }

    #[test]
    fn a_pin_is_thread_local_and_restores_on_drop() {
        let before = active();
        {
            let _pinned = pin(Skin::Codex);
            assert_eq!(active(), Skin::Codex);
            {
                let _nested = pin(Skin::Grok);
                assert_eq!(active(), Skin::Grok);
            }
            assert_eq!(active(), Skin::Codex);
        }
        assert_eq!(active(), before);
    }

    #[test]
    fn every_skin_names_a_theme_that_loads() {
        for skin in Skin::ALL {
            let name = skin.companion_theme();
            assert!(
                crate::theme::load(name).is_ok(),
                "{} names theme '{name}', which does not load",
                skin.key()
            );
        }
    }
}
