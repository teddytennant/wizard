//! The two typefaces the window draws in, embedded in the binary.
//!
//! `docs/gui-design-spec.md` is unusually blunt about this — "**type is
//! bundled, not hoped for**" — and Phase 1 shipped without it, so the window
//! rendered in whatever the system offered. On a plain Linux box that is DejaVu
//! Sans, and it shows: DejaVu is a 2004 typeface with no tabular figures, so a
//! token count and a relative age changed *width* as they ticked, and the whole
//! surface read as a dialog box rather than as an instrument.
//!
//! # Why this is not `Font::MONOSPACE`
//!
//! iced has two font sentinels, [`Font::DEFAULT`] and [`Font::MONOSPACE`], and
//! only the first is settable: `.default_font(…)` on the application builder
//! replaces `Font::DEFAULT`, while `Font::MONOSPACE` resolves through
//! cosmic-text's generic `monospace` family to whatever fontconfig nominates.
//! There is no hook in iced 0.14 to move it. So the transcript, the composer and
//! the markdown renderer name [`MONO`] rather than `Font::MONOSPACE`, and the
//! sans side stays on `Font::DEFAULT` because that one *is* redirected.
//!
//! The consequence worth stating: a `Font::MONOSPACE` left anywhere in this
//! crate's window code is a silent fall back to DejaVu Sans Mono beside
//! JetBrains Mono, which is exactly the failure this module exists to end.
//! `src/plugins/native/tests.rs` asserts that no block the transcript produces carries
//! it.
//!
//! # Variable, and why that matters here
//!
//! Both faces are variable along `wght`, and cosmic-text sets that axis from
//! the weight a span asks for (`cosmic_text::swash` reads the `wght` variation
//! and clamps the request into its range). One file therefore covers 400–700
//! with real interpolated weights rather than a synthetic emboldening, which is
//! what lets `**bold**` in a reply and a section label at weight 600 both be
//! honest without shipping five files.
//!
//! # Headless
//!
//! [`SETTINGS`] is the same pair handed to [`iced_test::Simulator`], so a test
//! rasterizes with the fonts the window rasterizes with. Without it a pixel
//! snapshot would be a photograph of whichever machine ran it, which is why
//! Phase 1's snapshot had to be structural.

use iced::Font;

/// Inter, the UI and prose face. Variable 400–700, latin subset.
pub const INTER: &[u8] = include_bytes!("../../../assets/fonts/inter.ttf");

/// JetBrains Mono, for literals. Variable 400–600, latin subset.
pub const JETBRAINS_MONO: &[u8] = include_bytes!("../../../assets/fonts/jetbrains-mono.ttf");

/// The family name inside `inter.ttf`, as fontdb will register it.
pub const SANS_FAMILY: &str = "Inter";

/// The family name inside `jetbrains-mono.ttf`.
pub const MONO_FAMILY: &str = "JetBrains Mono";

/// The mono face, by name.
///
/// A `const` rather than a `Font::MONOSPACE`: see the module header. Everything
/// in this window that draws a path, a command, a model tag or a branch name
/// uses this one.
pub const MONO: Font = Font::with_name(MONO_FAMILY);

/// The sans face, by name.
///
/// Also available as `Font::DEFAULT` once the application has been given
/// [`SETTINGS`], but naming it explicitly is what lets a widget built for a
/// test — which may not have gone through the builder — still get Inter.
pub const SANS: Font = Font::with_name(SANS_FAMILY);

/// The renderer settings both the window and the headless tests run under.
pub fn settings() -> iced::Settings {
    iced::Settings {
        default_font: SANS,
        fonts: vec![INTER.into(), JETBRAINS_MONO.into()],
        ..iced::Settings::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bytes have to be a font a face database will actually accept. An
    /// `include_bytes!` of the woff2 next door would compile perfectly and then
    /// render nothing, because fontdb reads TrueType and OpenType and woff2 is
    /// neither — which is the mistake this test exists to make loud.
    #[test]
    fn both_faces_parse_and_carry_the_family_names_the_window_asks_for() {
        for (bytes, family) in [(INTER, SANS_FAMILY), (JETBRAINS_MONO, MONO_FAMILY)] {
            let mut database = iced::advanced::graphics::text::cosmic_text::fontdb::Database::new();
            database.load_font_data(bytes.to_vec());
            let faces: Vec<String> = database
                .faces()
                .flat_map(|face| face.families.iter().map(|(name, _)| name.clone()))
                .collect();
            assert!(
                faces.iter().any(|name| name == family),
                "{family} is not among {faces:?}"
            );
        }
    }

    /// The four-byte tags in a font's table directory.
    ///
    /// Read here rather than through a parser because the two facts this file
    /// needs are facts about the *container*: that it is sfnt (and so not the
    /// woff2 it was decompressed from) and that it carries `fvar`.
    fn tables(bytes: &[u8]) -> Vec<String> {
        assert_eq!(&bytes[..4], b"\x00\x01\x00\x00", "not a TrueType sfnt");
        let count = u16::from_be_bytes([bytes[4], bytes[5]]) as usize;
        (0..count)
            .map(|index| {
                let at = 12 + index * 16;
                String::from_utf8_lossy(&bytes[at..at + 4]).into_owned()
            })
            .collect()
    }

    /// Variable, along `wght`. If a static face were ever swapped in here, bold
    /// text would quietly stop being bold — cosmic-text would find one weight,
    /// pick it for every request, and nothing would fail.
    #[test]
    fn both_faces_are_variable() {
        for bytes in [INTER, JETBRAINS_MONO] {
            let tables = tables(bytes);
            assert!(tables.contains(&"fvar".to_string()), "{tables:?}");
            assert!(tables.contains(&"glyf".to_string()), "{tables:?}");
        }
    }

    /// The settings the window runs under carry both files and default to the
    /// sans face. A `default_font` left at `Font::DEFAULT` would silently keep
    /// the system UI font for every paragraph of prose.
    #[test]
    fn the_settings_load_both_faces_and_default_to_inter() {
        let settings = settings();
        assert_eq!(settings.default_font, SANS);
        assert_eq!(settings.fonts.len(), 2);
    }
}
