//! opencode-compatible themes.
//!
//! The JSON format is the one opencode ships in `theme/assets/*.json`: a `defs` map of named
//! colours, a `theme` map of tokens whose values are a hex string, a `defs`/token reference,
//! an ANSI index, `"none"`, or a `{ "dark": .., "light": .. }` pair. Resolution rules follow
//! `resolveTheme` in opencode's `theme/index.ts`.

use anyhow::{anyhow, bail, Context, Result};
use ratatui::style::Color;
use serde::Deserialize;
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Dark,
    Light,
}

impl Mode {
    pub fn toggled(self) -> Mode {
        match self {
            Mode::Dark => Mode::Light,
            Mode::Light => Mode::Dark,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
enum ColorValue {
    Str(String),
    Num(i64),
    Variant {
        dark: Box<ColorValue>,
        light: Box<ColorValue>,
    },
}

#[derive(Clone, Debug, Deserialize)]
struct ThemeFile {
    #[serde(default)]
    defs: BTreeMap<String, ColorValue>,
    theme: BTreeMap<String, serde_json::Value>,
}

macro_rules! tokens {
    ($($field:ident => $key:literal),* $(,)?) => {
        /// A resolved theme. Colours are `Color::Rgb`, or `Color::Reset` for `none`.
        #[derive(Clone, Debug)]
        pub struct Theme {
            pub name: String,
            pub mode: Mode,
            $(pub $field: Color,)*
            /// Alpha applied to reasoning text over the background.
            pub thinking_opacity: f32,
            /// Alpha of the black wash behind dialogs (opencode uses 150/255).
            pub overlay_alpha: f32,
            has_selected_text: bool,
            source: Arc<ThemeFile>,
        }

        /// JSON keys of every token that must be present, in struct order.
        const REQUIRED: &[&str] = &[$($key),*];

        impl Theme {
            fn resolve_file(name: &str, file: &Arc<ThemeFile>, mode: Mode) -> Result<Theme> {
                let r = Resolver { file, mode };
                $(let $field = r.token($key)?;)*
                let has_selected_text = file.theme.contains_key("selectedListItemText");
                let thinking_opacity = file
                    .theme
                    .get("thinkingOpacity")
                    .and_then(|v| v.as_f64())
                    .map_or(0.6, |v| v as f32);
                let mut t = Theme {
                    name: name.to_string(),
                    mode,
                    $($field,)*
                    thinking_opacity,
                    overlay_alpha: 150.0 / 255.0,
                    has_selected_text,
                    source: file.clone(),
                };
                // Optional tokens: same fallbacks as opencode's resolveTheme.
                if !has_selected_text {
                    t.selected_list_item_text = t.background;
                }
                if !file.theme.contains_key("backgroundMenu") {
                    t.background_menu = t.background_element;
                }
                Ok(t)
            }
        }
    };
}

tokens! {
    primary => "primary",
    secondary => "secondary",
    accent => "accent",
    error => "error",
    warning => "warning",
    success => "success",
    info => "info",
    text => "text",
    text_muted => "textMuted",
    selected_list_item_text => "selectedListItemText",
    background => "background",
    background_panel => "backgroundPanel",
    background_element => "backgroundElement",
    background_menu => "backgroundMenu",
    border => "border",
    border_active => "borderActive",
    border_subtle => "borderSubtle",
    diff_added => "diffAdded",
    diff_removed => "diffRemoved",
    diff_context => "diffContext",
    diff_hunk_header => "diffHunkHeader",
    diff_highlight_added => "diffHighlightAdded",
    diff_highlight_removed => "diffHighlightRemoved",
    diff_added_bg => "diffAddedBg",
    diff_removed_bg => "diffRemovedBg",
    diff_context_bg => "diffContextBg",
    diff_line_number => "diffLineNumber",
    diff_added_line_number_bg => "diffAddedLineNumberBg",
    diff_removed_line_number_bg => "diffRemovedLineNumberBg",
    markdown_text => "markdownText",
    markdown_heading => "markdownHeading",
    markdown_link => "markdownLink",
    markdown_link_text => "markdownLinkText",
    markdown_code => "markdownCode",
    markdown_block_quote => "markdownBlockQuote",
    markdown_emph => "markdownEmph",
    markdown_strong => "markdownStrong",
    markdown_horizontal_rule => "markdownHorizontalRule",
    markdown_list_item => "markdownListItem",
    markdown_list_enumeration => "markdownListEnumeration",
    markdown_image => "markdownImage",
    markdown_image_text => "markdownImageText",
    markdown_code_block => "markdownCodeBlock",
    syntax_comment => "syntaxComment",
    syntax_keyword => "syntaxKeyword",
    syntax_function => "syntaxFunction",
    syntax_variable => "syntaxVariable",
    syntax_string => "syntaxString",
    syntax_number => "syntaxNumber",
    syntax_type => "syntaxType",
    syntax_operator => "syntaxOperator",
    syntax_punctuation => "syntaxPunctuation",
}

struct Resolver<'a> {
    file: &'a ThemeFile,
    mode: Mode,
}

impl Resolver<'_> {
    fn token(&self, key: &str) -> Result<Color> {
        // The three optional keys fall back after resolution, so give them a placeholder.
        let Some(v) = self.file.theme.get(key) else {
            return match key {
                "selectedListItemText" | "backgroundMenu" => Ok(Color::Reset),
                _ => bail!("theme is missing token \"{key}\""),
            };
        };
        let cv: ColorValue = serde_json::from_value(v.clone())
            .with_context(|| format!("token \"{key}\" is not a colour"))?;
        self.resolve(&cv, &mut Vec::new())
            .with_context(|| format!("resolving token \"{key}\""))
    }

    fn resolve(&self, cv: &ColorValue, chain: &mut Vec<String>) -> Result<Color> {
        match cv {
            ColorValue::Num(n) => Ok(ansi_index_color(*n)),
            ColorValue::Variant { dark, light } => {
                let pick = if self.mode == Mode::Dark { dark } else { light };
                self.resolve(pick, chain)
            }
            ColorValue::Str(s) => {
                if s == "transparent" || s == "none" {
                    return Ok(Color::Reset);
                }
                if s.starts_with('#') {
                    return parse_hex(s);
                }
                if chain.iter().any(|c| c == s) {
                    bail!("circular colour reference: {} -> {s}", chain.join(" -> "));
                }
                let next: ColorValue = if let Some(d) = self.file.defs.get(s) {
                    d.clone()
                } else if let Some(t) = self.file.theme.get(s) {
                    serde_json::from_value(t.clone())
                        .with_context(|| format!("reference \"{s}\" is not a colour"))?
                } else {
                    bail!("colour reference \"{s}\" not found in defs or theme");
                };
                chain.push(s.clone());
                let out = self.resolve(&next, chain);
                chain.pop();
                out
            }
        }
    }
}

fn parse_hex(s: &str) -> Result<Color> {
    let h = s.trim_start_matches('#');
    let nib = |c: char| {
        c.to_digit(16)
            .map(|d| d as u8)
            .ok_or_else(|| anyhow!("bad hex colour \"{s}\""))
    };
    let chars: Vec<char> = h.chars().collect();
    let (r, g, b, a) = match chars.len() {
        3 => (
            nib(chars[0])? * 17,
            nib(chars[1])? * 17,
            nib(chars[2])? * 17,
            255,
        ),
        6 | 8 => {
            let byte = |i: usize| -> Result<u8> { Ok(nib(chars[i])? * 16 + nib(chars[i + 1])?) };
            (
                byte(0)?,
                byte(2)?,
                byte(4)?,
                if chars.len() == 8 { byte(6)? } else { 255 },
            )
        }
        _ => bail!("bad hex colour \"{s}\""),
    };
    // Fully transparent hex behaves like "none"; partial alpha has no terminal equivalent.
    Ok(if a == 0 {
        Color::Reset
    } else {
        Color::Rgb(r, g, b)
    })
}

/// Same table opencode uses for numeric colour values.
fn ansi_index_color(code: i64) -> Color {
    const BASE: [(u8, u8, u8); 16] = [
        (0x00, 0x00, 0x00),
        (0x80, 0x00, 0x00),
        (0x00, 0x80, 0x00),
        (0x80, 0x80, 0x00),
        (0x00, 0x00, 0x80),
        (0x80, 0x00, 0x80),
        (0x00, 0x80, 0x80),
        (0xc0, 0xc0, 0xc0),
        (0x80, 0x80, 0x80),
        (0xff, 0x00, 0x00),
        (0x00, 0xff, 0x00),
        (0xff, 0xff, 0x00),
        (0x00, 0x00, 0xff),
        (0xff, 0x00, 0xff),
        (0x00, 0xff, 0xff),
        (0xff, 0xff, 0xff),
    ];
    match code {
        0..=15 => {
            let (r, g, b) = BASE[code as usize];
            Color::Rgb(r, g, b)
        }
        16..=231 => {
            let i = (code - 16) as u8;
            let v = |x: u8| if x == 0 { 0 } else { x * 40 + 55 };
            Color::Rgb(v(i / 36), v((i / 6) % 6), v(i % 6))
        }
        232..=255 => {
            let g = ((code - 232) * 10 + 8) as u8;
            Color::Rgb(g, g, g)
        }
        _ => Color::Rgb(0, 0, 0),
    }
}

macro_rules! builtin_themes {
    ($($name:literal),* $(,)?) => {
        const BUILTIN: &[(&str, &str)] = &[
            $(($name, include_str!(concat!("../themes/", $name, ".json")))),*
        ];
    };
}

builtin_themes! {
    "aura", "ayu", "carbonfox", "catppuccin-frappe", "catppuccin-macchiato", "catppuccin",
    "cobalt2", "cursor", "dracula", "everforest", "flexoki", "github", "gruvbox", "kanagawa",
    "lucent-orng", "material", "matrix", "mercury", "monokai", "nightowl", "nord", "one-dark",
    "opencode", "orng", "osaka-jade", "palenight", "rosepine", "solarized", "synthwave84",
    "tokyonight", "vercel", "vesper", "zenburn",
}

/// Name opencode starts with.
pub const DEFAULT_THEME: &str = "opencode";

/// RGB triple for any colour we can compute with. `Reset` has none.
pub fn rgb_of(c: Color) -> Option<(u8, u8, u8)> {
    match c {
        Color::Rgb(r, g, b) => Some((r, g, b)),
        Color::Indexed(n) => match ansi_index_color(n as i64) {
            Color::Rgb(r, g, b) => Some((r, g, b)),
            _ => None,
        },
        Color::Black => Some((0, 0, 0)),
        Color::Red => Some((0x80, 0, 0)),
        Color::Green => Some((0, 0x80, 0)),
        Color::Yellow => Some((0x80, 0x80, 0)),
        Color::Blue => Some((0, 0, 0x80)),
        Color::Magenta => Some((0x80, 0, 0x80)),
        Color::Cyan => Some((0, 0x80, 0x80)),
        Color::Gray => Some((0xc0, 0xc0, 0xc0)),
        Color::DarkGray => Some((0x80, 0x80, 0x80)),
        Color::LightRed => Some((0xff, 0, 0)),
        Color::LightGreen => Some((0, 0xff, 0)),
        Color::LightYellow => Some((0xff, 0xff, 0)),
        Color::LightBlue => Some((0, 0, 0xff)),
        Color::LightMagenta => Some((0xff, 0, 0xff)),
        Color::LightCyan => Some((0, 0xff, 0xff)),
        Color::White => Some((0xff, 0xff, 0xff)),
        Color::Reset => None,
    }
}

impl Theme {
    /// Names of all embedded themes, sorted.
    pub fn names() -> Vec<&'static str> {
        let mut v: Vec<&'static str> = BUILTIN.iter().map(|(n, _)| *n).collect();
        v.sort_unstable();
        v
    }

    /// Dark variant of an embedded theme.
    pub fn builtin(name: &str) -> Option<Theme> {
        Self::builtin_mode(name, Mode::Dark)
    }

    pub fn builtin_mode(name: &str, mode: Mode) -> Option<Theme> {
        let (n, json) = BUILTIN.iter().find(|(n, _)| *n == name)?;
        Self::from_json_named(n, json, mode).ok()
    }

    /// The default theme. Panics only if the embedded opencode.json is broken, which a unit
    /// test rules out.
    pub fn default_theme(mode: Mode) -> Theme {
        Self::builtin_mode(DEFAULT_THEME, mode).expect("embedded opencode theme parses")
    }

    /// Parse and resolve an arbitrary theme file (for user themes).
    pub fn from_json(json: &str, mode: Mode) -> Result<Theme> {
        Self::from_json_named("custom", json, mode)
    }

    pub fn from_json_named(name: &str, json: &str, mode: Mode) -> Result<Theme> {
        let file: ThemeFile = serde_json::from_str(json).context("theme json")?;
        Self::resolve_file(name, &Arc::new(file), mode)
    }

    /// Same theme in the other (or a given) mode, re-resolved from the source file.
    pub fn with_mode(&self, mode: Mode) -> Theme {
        Self::resolve_file(&self.name, &self.source, mode).unwrap_or_else(|_| self.clone())
    }

    /// The same theme with `fg` fixed as the text colour on selected rows. opencode computes it
    /// once when a dialog opens, so a dialog that previews other themes keeps the opening one.
    pub fn with_selected_text(&self, fg: Color) -> Theme {
        let mut t = self.clone();
        t.selected_list_item_text = fg;
        t.has_selected_text = true;
        t
    }

    pub fn toggled(&self) -> Theme {
        self.with_mode(self.mode.toggled())
    }

    /// Foreground for text drawn on a selected row. Themes that define
    /// `selectedListItemText` use it; the rest use the page background, like opencode.
    /// `bg` is the colour the text sits on and only matters for transparent backgrounds.
    pub fn selected_foreground(&self, bg: Option<Color>) -> Color {
        if self.has_selected_text {
            return self.selected_list_item_text;
        }
        if self.background == Color::Reset {
            let target = bg.unwrap_or(self.primary);
            if let Some((r, g, b)) = rgb_of(target) {
                let lum = 0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32;
                return if lum / 255.0 > 0.5 {
                    Color::Rgb(0, 0, 0)
                } else {
                    Color::Rgb(255, 255, 255)
                };
            }
        }
        self.background
    }

    /// Colour to blend against when a token is transparent.
    pub fn base_bg(&self) -> Color {
        if self.background != Color::Reset {
            return self.background;
        }
        match self.mode {
            Mode::Dark => Color::Rgb(0, 0, 0),
            Mode::Light => Color::Rgb(255, 255, 255),
        }
    }

    /// `fg` drawn at opacity `a` over `bg`: `bg + (fg - bg) * a`. Falls back to `fg` when either
    /// side has no RGB value (a transparent background, say).
    pub fn alpha_over(fg: Color, bg: Color, a: f32) -> Color {
        let (Some((fr, fg_, fb)), Some((br, bg_, bb))) = (rgb_of(fg), rgb_of(bg)) else {
            return fg;
        };
        let a = a.clamp(0.0, 1.0);
        let mix = |f: u8, b: u8| {
            (b as f32 + (f as f32 - b as f32) * a)
                .round()
                .clamp(0.0, 255.0) as u8
        };
        Color::Rgb(mix(fr, br), mix(fg_, bg_), mix(fb, bb))
    }

    /// Reasoning text colour: muted text at `thinking_opacity` over the background.
    pub fn thinking_color(&self) -> Color {
        Self::alpha_over(self.text_muted, self.base_bg(), self.thinking_opacity)
    }

    /// Dim a colour the way the dialog backdrop does (black at `overlay_alpha`). Transparent
    /// colours become the dimmed base background.
    pub fn dim(&self, c: Color) -> Color {
        let c = if c == Color::Reset { self.base_bg() } else { c };
        Self::alpha_over(Color::Rgb(0, 0, 0), c, self.overlay_alpha)
    }

    /// Semantic colour by variant name, for toasts and status text.
    pub fn variant(&self, v: Variant) -> Color {
        match v {
            Variant::Info => self.info,
            Variant::Success => self.success,
            Variant::Warning => self.warning,
            Variant::Error => self.error,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Variant {
    Info,
    Success,
    Warning,
    Error,
}

/// Names of tokens every theme must define; handy for validating user themes.
pub fn required_tokens() -> HashSet<&'static str> {
    REQUIRED
        .iter()
        .copied()
        .filter(|k| *k != "selectedListItemText" && *k != "backgroundMenu")
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_embedded_theme_resolves_in_both_modes() {
        let names = Theme::names();
        assert!(
            names.len() >= 30,
            "expected the full opencode set, got {}",
            names.len()
        );
        for n in names {
            for mode in [Mode::Dark, Mode::Light] {
                let t =
                    Theme::builtin_mode(n, mode).unwrap_or_else(|| panic!("{n} {mode:?} failed"));
                assert_eq!(t.name, n);
            }
        }
    }

    #[test]
    fn opencode_dark_matches_the_reference_screenshot_colours() {
        let t = Theme::builtin("opencode").unwrap();
        assert_eq!(t.background, Color::Rgb(0x0a, 0x0a, 0x0a));
        assert_eq!(t.background_panel, Color::Rgb(0x14, 0x14, 0x14));
        assert_eq!(t.background_element, Color::Rgb(0x1e, 0x1e, 0x1e));
        assert_eq!(t.primary, Color::Rgb(0xfa, 0xb2, 0x83));
        assert_eq!(t.text, Color::Rgb(0xee, 0xee, 0xee));
        assert_eq!(t.text_muted, Color::Rgb(0x80, 0x80, 0x80));
        assert!((t.thinking_opacity - 0.6).abs() < 1e-6);
    }

    #[test]
    fn light_mode_differs_and_toggle_round_trips() {
        let d = Theme::builtin("opencode").unwrap();
        let l = d.toggled();
        assert_eq!(l.mode, Mode::Light);
        assert_ne!(d.background, l.background);
        assert_eq!(l.toggled().background, d.background);
    }

    #[test]
    fn refs_variants_ansi_and_none() {
        let json = r##"{
          "defs": { "a": "#102030", "b": "a", "gray": 244 },
          "theme": { "primary": {"dark": "b", "light": "#ffffff"},
                     "secondary": "gray", "accent": "none", "error": "primary" }
        }"##;
        let file: ThemeFile = serde_json::from_str(json).unwrap();
        let r = Resolver {
            file: &file,
            mode: Mode::Dark,
        };
        assert_eq!(r.token("primary").unwrap(), Color::Rgb(0x10, 0x20, 0x30));
        assert_eq!(r.token("secondary").unwrap(), Color::Rgb(128, 128, 128));
        assert_eq!(r.token("accent").unwrap(), Color::Reset);
        // Token referring to another token.
        assert_eq!(r.token("error").unwrap(), Color::Rgb(0x10, 0x20, 0x30));
        let light = Resolver {
            file: &file,
            mode: Mode::Light,
        };
        assert_eq!(light.token("primary").unwrap(), Color::Rgb(255, 255, 255));
    }

    #[test]
    fn ansi_cube_and_gray_ramp() {
        assert_eq!(ansi_index_color(1), Color::Rgb(0x80, 0, 0));
        assert_eq!(ansi_index_color(16), Color::Rgb(0, 0, 0));
        assert_eq!(ansi_index_color(196), Color::Rgb(255, 0, 0));
        assert_eq!(ansi_index_color(232), Color::Rgb(8, 8, 8));
        assert_eq!(ansi_index_color(999), Color::Rgb(0, 0, 0));
    }

    #[test]
    fn circular_and_missing_references_are_errors() {
        let circ = r##"{"defs":{"a":"b","b":"a"},"theme":{"primary":"a"}}"##;
        let file: ThemeFile = serde_json::from_str(circ).unwrap();
        let err = Resolver {
            file: &file,
            mode: Mode::Dark,
        }
        .token("primary")
        .unwrap_err();
        assert!(format!("{err:#}").contains("circular"), "{err:#}");
        let missing = r##"{"theme":{"primary":"nope"}}"##;
        let file: ThemeFile = serde_json::from_str(missing).unwrap();
        let err = Resolver {
            file: &file,
            mode: Mode::Dark,
        }
        .token("primary")
        .unwrap_err();
        assert!(format!("{err:#}").contains("not found"));
        assert!(Theme::from_json(missing, Mode::Dark).is_err());
        assert!(Theme::from_json("not json", Mode::Dark).is_err());
    }

    #[test]
    fn selected_list_item_text_and_menu_fall_back() {
        let t = Theme::builtin("opencode").unwrap();
        // opencode.json defines neither, so both fall back like opencode does.
        assert_eq!(t.selected_foreground(None), t.background);
        assert_eq!(t.background_menu, t.background_element);
    }

    #[test]
    fn alpha_over_blends_and_degrades() {
        let w = Color::Rgb(255, 255, 255);
        let b = Color::Rgb(0, 0, 0);
        assert_eq!(Theme::alpha_over(w, b, 0.5), Color::Rgb(128, 128, 128));
        assert_eq!(Theme::alpha_over(w, b, 0.0), b);
        assert_eq!(Theme::alpha_over(w, b, 1.0), w);
        assert_eq!(Theme::alpha_over(w, b, 7.0), w);
        assert_eq!(Theme::alpha_over(w, Color::Reset, 0.5), w);
    }

    #[test]
    fn dim_matches_opencode_overlay() {
        let t = Theme::builtin("opencode").unwrap();
        // 0x1e at 150/255 black overlay: 30 * (1 - 0.588) = 12.4 -> 12
        assert_eq!(t.dim(Color::Rgb(30, 30, 30)), Color::Rgb(12, 12, 12));
        assert_eq!(t.dim(Color::Reset), Color::Rgb(4, 4, 4));
    }

    #[test]
    fn hex_forms() {
        assert_eq!(parse_hex("#fff").unwrap(), Color::Rgb(255, 255, 255));
        assert_eq!(
            parse_hex("#10203040").unwrap(),
            Color::Rgb(0x10, 0x20, 0x30)
        );
        assert_eq!(parse_hex("#00000000").unwrap(), Color::Reset);
        assert!(parse_hex("#12").is_err());
        assert!(parse_hex("#gggggg").is_err());
    }

    #[test]
    fn lucent_orng_uses_none_without_failing() {
        let t = Theme::builtin("lucent-orng").unwrap();
        assert_eq!(t.background, Color::Reset);
        // Blending against a transparent background must still produce a colour.
        assert!(matches!(t.thinking_color(), Color::Rgb(..)));
        assert!(matches!(t.dim(t.background), Color::Rgb(..)));
    }
}
