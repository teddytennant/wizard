//! One palette, two renderers.
//!
//! [`crate::theme`] already answers "what colour is a tool that failed" for the
//! TUI, and it answers it semantically: renderers ask for a [`Token`] and only
//! theme *data* ever names a literal colour. The native GUI is a second
//! renderer, and the whole reason that token layer exists is so it does not get
//! a palette of its own — a `minimal` window and a `minimal` terminal disagreeing
//! about what an accent is would be two products wearing one name.
//!
//! So this module is a translation and not a design. Everything here falls into
//! exactly one of two buckets, and the split is what keeps it honest:
//!
//! - **Token colours**, which come from the active theme and nowhere else, via
//!   [`Theme::declared`] — the value the theme file wrote, *before* the
//!   terminal's colour-depth adaptation. A window is not a 16-colour terminal,
//!   so degrading for one would be answering a question nobody asked.
//! - **Chrome**, which the token layer deliberately does not carry: canvas,
//!   surface, hairline, the selection wash. The TUI paints on the terminal's own
//!   background and so has no opinion about any of them (see the "no background
//!   colors anywhere" rule in `assets/themes/minimal.toml`), while a window has
//!   to put *something* behind the text. Those four values come from
//!   `docs/gui-design-spec.md`, which is the only place they are specified.
//!
//! # `reset`, and why it needs an answer here
//!
//! Both built-in themes set `text = "reset"`, meaning *the terminal's own
//! foreground* — a deferral that works because a terminal always has one. A
//! window does not: there is no ambient foreground to defer to, and a
//! [`ratatui::style::Color::Reset`] handed to a rasterizer is not a colour at
//! all. [`resolve`] turns it into the spec's primary text on the spec's canvas,
//! which is the same *intent* (body text at full contrast against whatever the
//! surface is) expressed in the terms this renderer has.
//!
//! # ANSI names are real colours here
//!
//! A theme is free to say `gray` or `9` or `#3fb96a`, and the first two mean
//! whatever the terminal emulator decides. A window has to decide. [`ANSI16`]
//! and [`xterm256`] are the standard xterm palette, so `minimal` — which is
//! written entirely in ANSI-16 names on purpose, to survive SSH — renders as the
//! same greys people already see in their terminal rather than as a second
//! interpretation of the same words.

use iced::Color;
use ratatui::style::Color as TermColor;

use crate::theme::{Theme, Token};

/// Canvas. `docs/gui-design-spec.md`, "Global".
const CANVAS: Color = rgb(0x0c, 0x0c, 0x0e);
/// The raised surface a composer or a code block sits on.
const SURFACE: Color = rgb(0x14, 0x14, 0x16);
/// A surface one step further forward (a tool row's body).
const RAISED: Color = rgb(0x19, 0x1a, 0x1d);
/// Hairline between regions.
const HAIRLINE: Color = rgb(0x26, 0x26, 0x2a);
/// A divider *inside* a surface: felt, not seen.
const SEPARATOR: Color = rgb(0x1f, 0x1f, 0x23);
/// Primary text, and what `reset` resolves to.
const PRIMARY_TEXT: Color = rgb(0xec, 0xec, 0xee);

/// The selection wash, drawn under the glyphs.
///
/// Deliberately not a token: selection is a property of this renderer (the TUI
/// has no drag-select at all), and it has to be a *translucent* fill or it would
/// hide the text it is meant to be highlighting. Alpha is what makes one colour
/// work over prose, over a code block's surface and over a tool row's without
/// three cases.
const SELECTION: Color = Color::from_rgba(0.42, 0.55, 0.78, 0.34);

/// A const `Color` from 8-bit channels.
///
/// `Color::from_rgb8` is not const in iced 0.14, and these are constants.
const fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color {
        r: r as f32 / 255.0,
        g: g as f32 / 255.0,
        b: b as f32 / 255.0,
        a: 1.0,
    }
}

/// The standard xterm ANSI-16 palette, in ratatui's `Color` order: the eight
/// normal colours then the eight bright ones.
const ANSI16: [Color; 16] = [
    rgb(0x00, 0x00, 0x00), // black
    rgb(0xcd, 0x00, 0x00), // red
    rgb(0x00, 0xcd, 0x00), // green
    rgb(0xcd, 0xcd, 0x00), // yellow
    rgb(0x00, 0x00, 0xee), // blue
    rgb(0xcd, 0x00, 0xcd), // magenta
    rgb(0x00, 0xcd, 0xcd), // cyan
    rgb(0xe5, 0xe5, 0xe5), // white (ratatui: Gray)
    rgb(0x7f, 0x7f, 0x7f), // bright black (ratatui: DarkGray)
    rgb(0xff, 0x00, 0x00), // bright red
    rgb(0x00, 0xff, 0x00), // bright green
    rgb(0xff, 0xff, 0x00), // bright yellow
    rgb(0x5c, 0x5c, 0xff), // bright blue
    rgb(0xff, 0x00, 0xff), // bright magenta
    rgb(0x00, 0xff, 0xff), // bright cyan
    rgb(0xff, 0xff, 0xff), // bright white
];

/// The xterm 256-colour cube and greyscale ramp, for an `Indexed` token.
///
/// Indices 0–15 defer to [`ANSI16`], 16–231 are the 6×6×6 cube, 232–255 the
/// 24-step grey ramp. This is the same arithmetic every terminal emulator
/// implements, written out rather than tabulated.
fn xterm256(index: u8) -> Color {
    match index {
        0..=15 => ANSI16[index as usize],
        16..=231 => {
            const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
            let n = index - 16;
            rgb(
                LEVELS[(n / 36) as usize],
                LEVELS[(n % 36 / 6) as usize],
                LEVELS[(n % 6) as usize],
            )
        }
        232..=255 => {
            let level = 8 + (index - 232) * 10;
            rgb(level, level, level)
        }
    }
}

/// One theme's tokens as a window can use them, plus the chrome the token layer
/// does not carry.
///
/// Built once per theme change rather than per draw: resolving nineteen tokens
/// is cheap, but `view()` runs on every event and a palette that is rebuilt
/// there is nineteen lookups and a lock per frame for a value that changes when
/// the user types `/theme` and at no other time.
#[derive(Debug, Clone, PartialEq)]
pub struct Palette {
    /// The theme this was built from, so a change is detectable without
    /// comparing nineteen colours.
    pub name: String,
    tokens: [Color; Token::ALL.len()],
    pub canvas: Color,
    pub surface: Color,
    pub raised: Color,
    pub hairline: Color,
    /// A divider *inside* a surface — "felt, not seen", per the design spec.
    /// The turn marker in the transcript is the only one this phase draws.
    pub separator: Color,
    pub selection: Color,
}

impl Palette {
    /// Translate `theme`'s declared tokens into window colours.
    pub fn from_theme(theme: &Theme) -> Self {
        let mut tokens = [PRIMARY_TEXT; Token::ALL.len()];
        for token in Token::ALL {
            tokens[token as usize] = resolve(theme.declared(token));
        }
        Self {
            name: theme.name.clone(),
            tokens,
            canvas: CANVAS,
            surface: SURFACE,
            raised: RAISED,
            hairline: HAIRLINE,
            separator: SEPARATOR,
            selection: SELECTION,
        }
    }

    /// The palette for whichever theme is currently active process-wide, which
    /// is the one `/theme` and `WIZARD_THEME` both move.
    pub fn active() -> Self {
        Self::from_theme(&crate::theme::active())
    }

    /// The colour for `token`.
    pub fn color(&self, token: Token) -> Color {
        self.tokens[token as usize]
    }
}

/// One terminal colour as a window colour.
///
/// `Reset` is the interesting case and it is documented in the module header:
/// there is no ambient foreground behind a rasterizer, so the deferral has to
/// resolve to something, and the something is the design spec's primary text.
fn resolve(color: TermColor) -> Color {
    match color {
        TermColor::Reset => PRIMARY_TEXT,
        TermColor::Rgb(r, g, b) => rgb(r, g, b),
        TermColor::Indexed(index) => xterm256(index),
        TermColor::Black => ANSI16[0],
        TermColor::Red => ANSI16[1],
        TermColor::Green => ANSI16[2],
        TermColor::Yellow => ANSI16[3],
        TermColor::Blue => ANSI16[4],
        TermColor::Magenta => ANSI16[5],
        TermColor::Cyan => ANSI16[6],
        TermColor::Gray => ANSI16[7],
        TermColor::DarkGray => ANSI16[8],
        TermColor::LightRed => ANSI16[9],
        TermColor::LightGreen => ANSI16[10],
        TermColor::LightYellow => ANSI16[11],
        TermColor::LightBlue => ANSI16[12],
        TermColor::LightMagenta => ANSI16[13],
        TermColor::LightCyan => ANSI16[14],
        TermColor::White => ANSI16[15],
    }
}

/// The iced `Theme` the window runs under.
///
/// A custom palette rather than `Theme::Dark`, so the stock widgets this phase
/// still uses (the scrollable's rail, the composer's text input) sit on the same
/// canvas the transcript does instead of on iced's own near-black.
pub fn iced_theme(palette: &Palette) -> iced::Theme {
    iced::Theme::custom(
        format!("wizard-{}", palette.name),
        iced::theme::Palette {
            background: palette.canvas,
            text: palette.color(Token::Text),
            primary: palette.color(Token::Accent),
            success: palette.color(Token::Success),
            warning: palette.color(Token::Warning),
            danger: palette.color(Token::Error),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The point of the whole module: the window's colours come out of the
    /// theme file, so editing a theme moves both surfaces. If this ever passed
    /// by accident it would be because `minimal` and `codex` agreed, so it
    /// asserts on a token where they are documented to differ.
    #[test]
    fn two_themes_produce_two_palettes() {
        let minimal = Palette::from_theme(&crate::theme::minimal());
        let ember =
            Palette::from_theme(&crate::theme::load("codex").expect("codex ships in the binary"));
        assert_ne!(
            minimal.color(Token::Accent),
            ember.color(Token::Accent),
            "a themed accent that is the same in both themes is not themed"
        );
        // Chrome is not themed, and that is on purpose: it is not in the token
        // layer at all.
        assert_eq!(minimal.canvas, ember.canvas);
    }

    /// `reset` means "the terminal's foreground", and a window has none. It has
    /// to become a real colour or the transcript draws in whatever
    /// uninitialized value a `Color::Reset` degrades to.
    #[test]
    fn reset_resolves_to_a_real_foreground() {
        assert_eq!(resolve(TermColor::Reset), PRIMARY_TEXT);
        assert_eq!(
            Palette::from_theme(&crate::theme::minimal()).color(Token::Text),
            PRIMARY_TEXT,
            "minimal declares text = reset"
        );
    }

    /// The 256-colour arithmetic, at the three boundaries a theme can reach:
    /// the ANSI window, the first and last cell of the colour cube, and the
    /// grey ramp.
    #[test]
    fn indexed_colors_follow_the_xterm_palette() {
        assert_eq!(xterm256(1), ANSI16[1]);
        assert_eq!(xterm256(16), rgb(0, 0, 0));
        assert_eq!(xterm256(231), rgb(255, 255, 255));
        assert_eq!(xterm256(196), rgb(255, 0, 0));
        assert_eq!(xterm256(232), rgb(8, 8, 8));
        assert_eq!(xterm256(255), rgb(238, 238, 238));
    }

    /// The selection wash has to let the glyphs under it through. A fully
    /// opaque one would hide exactly the text a user selected in order to read.
    #[test]
    fn the_selection_wash_is_translucent() {
        let wash = Palette::from_theme(&crate::theme::minimal()).selection;
        assert!(wash.a > 0.0 && wash.a < 1.0, "{wash:?}");
    }

    /// The declared value, not the resolved one: a window is not a 16-colour
    /// terminal, so `codex` running in a `Mono` terminal must still be codex in
    /// its window.
    #[test]
    fn depth_adaptation_does_not_reach_the_window() {
        let ember = crate::theme::load("codex").expect("codex ships in the binary");
        let mono = ember.with_depth(crate::theme::ColorDepth::Mono);
        assert_eq!(
            Palette::from_theme(&ember).color(Token::Accent),
            Palette::from_theme(&mono).color(Token::Accent),
        );
    }
}
