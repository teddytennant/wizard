//! Which drawing the house skin uses for a handful of markers.
//!
//! The core set is what a stock terminal draws. A few emulators get a richer
//! one without anyone setting a variable, and `WIZARD_GLYPHS=ascii` drops to
//! ASCII when even the core set is more than the terminal will show. Codex
//! and Grok keep the glyphs in their own chrome tables; this only rewrites
//! the house skins.

use std::borrow::Cow;

use super::Skin;

/// Which glyph table is in force.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Set {
    /// `◆` `╰` `●` `✕` — the default.
    Core,
    /// `✦` `⎿` — kitty, Ghostty, WezTerm, iTerm2.
    Rich,
    /// `*` `` ` `` `o` `x` — `WIZARD_GLYPHS=ascii`.
    Ascii,
}

/// The set to draw with right now.
pub fn set() -> Set {
    if let Ok(value) = std::env::var("WIZARD_GLYPHS") {
        return match value.trim().to_ascii_lowercase().as_str() {
            "ascii" => Set::Ascii,
            "rich" => Set::Rich,
            "core" | "default" => Set::Core,
            _ => Set::Core,
        };
    }
    let program = std::env::var("TERM_PROGRAM").unwrap_or_default();
    let term = std::env::var("TERM")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let rich = matches!(
        program.as_str(),
        "kitty" | "WezTerm" | "iTerm.app" | "ghostty"
    ) || term.contains("ghostty");
    if rich { Set::Rich } else { Set::Core }
}

fn is_house() -> bool {
    matches!(super::active(), Skin::Wizard | Skin::Opencode | Skin::Pi)
}

/// Rewrite a house marker for the active set. Other skins get it back unchanged.
pub fn adapt(mark: &'static str) -> Cow<'static, str> {
    if !is_house() || set() == Set::Core {
        return Cow::Borrowed(mark);
    }
    Cow::Owned(rewrite(mark, set()))
}

/// The tool-output arm, same display width on every row.
pub fn tool_arm() -> (&'static str, &'static str) {
    let chrome = super::chrome().tool_output;
    if !is_house() {
        return chrome;
    }
    match set() {
        Set::Ascii => ("` ", "  "),
        Set::Rich => ("⎿ ", "  "),
        Set::Core => chrome,
    }
}

/// A spinner that does not move (`WIZARD_REDUCED_MOTION`).
pub fn still() -> char {
    match set() {
        Set::Ascii => '*',
        Set::Rich => '✦',
        Set::Core => '◆',
    }
}

/// Apply `set` to `mark`. Pure, so a test can pin a set without the environment.
pub fn rewrite(mark: &str, set: Set) -> String {
    mark.chars().map(|ch| map_glyph(ch, set)).collect()
}

fn map_glyph(ch: char, set: Set) -> char {
    match set {
        Set::Core => ch,
        Set::Rich => match ch {
            '◆' => '✦',
            '╰' => '⎿',
            _ => ch,
        },
        Set::Ascii => match ch {
            '◆' | '✦' => '*',
            '●' | '◉' | '○' => 'o',
            '✕' | '✗' => 'x',
            '✓' | '✔' => '+',
            '╰' | '⎿' => '`',
            '❯' | '▸' => '>',
            '▾' => 'v',
            '▍' | '│' => '|',
            _ => ch,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_and_rich_keep_the_marker_width() {
        for mark in ["◆ ", "╰ ", "●", "✕", "❯ ", "▸", "▾"] {
            for set in [Set::Core, Set::Rich, Set::Ascii] {
                let out = rewrite(mark, set);
                assert_eq!(
                    unicode_width::UnicodeWidthStr::width(mark),
                    unicode_width::UnicodeWidthStr::width(out.as_str()),
                    "{mark:?} -> {out:?} under {set:?}"
                );
            }
        }
        assert_eq!(rewrite("◆ ", Set::Ascii), "* ");
        assert_eq!(rewrite("╰ ", Set::Rich), "⎿ ");
        assert_eq!(rewrite("●", Set::Ascii), "o");
    }
}
