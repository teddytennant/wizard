// OWNER: theme (Pi theme loader, `system` generator)
//! Pi's theme tokens as ratatui styles.
//!
//! `dark` and `light` come from `crates/tuikit/themes-pi/resolved.json` (every token already
//! resolved to hex by Pi's own OKHSL code). `system` is Pi's default theme: generated from the
//! colours the terminal reports (`crate::system_theme`, a port of `generateSystemThemeColors`),
//! or, when it reports nothing, palette indices and the terminal default with neutral tokens
//! faint. A user theme file (`--theme <path>`) is read with hex, palette-index, `vars` and `""`
//! values; `oklch`/`okhsl` values fall back to `dark`.

use std::collections::HashSet;

use ratatui::style::{Color, Modifier, Style};
use serde_json::Value;
use tuikit::input::{ColorTarget, Reply};

use crate::system_theme::{self, Appearance, Gen, Generated, Reported, Rgb};

macro_rules! tokens {
    ($($variant:ident => $name:literal),+ $(,)?) => {
        /// A colour token of Pi's theme schema.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum Tok { $($variant),+ }
        pub const ALL_TOKENS: &[Tok] = &[$(Tok::$variant),+];
        impl Tok {
            pub fn name(self) -> &'static str {
                match self { $(Tok::$variant => $name),+ }
            }
        }
    };
}

tokens! {
    Accent => "accent",
    Border => "border",
    BorderAccent => "borderAccent",
    BorderMuted => "borderMuted",
    Success => "success",
    Error => "error",
    Warning => "warning",
    Muted => "muted",
    Dim => "dim",
    Text => "text",
    ThinkingText => "thinkingText",
    SelectedBg => "selectedBg",
    ScrollbarTrack => "scrollbarTrack",
    ScrollbarThumb => "scrollbarThumb",
    SearchMatchBg => "searchMatchBg",
    SearchMatchText => "searchMatchText",
    UserMessageBg => "userMessageBg",
    UserMessageText => "userMessageText",
    CustomMessageBg => "customMessageBg",
    CustomMessageText => "customMessageText",
    CustomMessageLabel => "customMessageLabel",
    ToolPendingBg => "toolPendingBg",
    ToolSuccessBg => "toolSuccessBg",
    ToolErrorBg => "toolErrorBg",
    ToolTitle => "toolTitle",
    ToolOutput => "toolOutput",
    MdHeading => "mdHeading",
    MdLink => "mdLink",
    MdLinkUrl => "mdLinkUrl",
    MdCode => "mdCode",
    MdCodeBlock => "mdCodeBlock",
    MdCodeBlockBorder => "mdCodeBlockBorder",
    MdQuote => "mdQuote",
    MdQuoteBorder => "mdQuoteBorder",
    MdHr => "mdHr",
    MdListBullet => "mdListBullet",
    ToolDiffAdded => "toolDiffAdded",
    ToolDiffRemoved => "toolDiffRemoved",
    ToolDiffContext => "toolDiffContext",
    SyntaxComment => "syntaxComment",
    SyntaxKeyword => "syntaxKeyword",
    SyntaxFunction => "syntaxFunction",
    SyntaxVariable => "syntaxVariable",
    SyntaxString => "syntaxString",
    SyntaxNumber => "syntaxNumber",
    SyntaxType => "syntaxType",
    SyntaxOperator => "syntaxOperator",
    SyntaxPunctuation => "syntaxPunctuation",
    ThinkingOff => "thinkingOff",
    ThinkingMinimal => "thinkingMinimal",
    ThinkingLow => "thinkingLow",
    ThinkingMedium => "thinkingMedium",
    ThinkingHigh => "thinkingHigh",
    ThinkingXhigh => "thinkingXhigh",
    ThinkingMax => "thinkingMax",
    BashMode => "bashMode",
}

/// One resolved token: a colour (`Color::Reset` is the terminal default) and whether it is drawn
/// faint (SGR 2), which only the `system` fallback tier does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Paint {
    pub color: Color,
    pub faint: bool,
}

impl Paint {
    const DEFAULT: Paint = Paint {
        color: Color::Reset,
        faint: false,
    };
}

/// The logo is fixed in every theme.
pub const LOGO_CORAL: Color = Color::Rgb(0xe4, 0x8a, 0x7a);
pub const LOGO_BLUE: Color = Color::Rgb(0x4f, 0x8e, 0xb3);
pub const LOGO_YELLOW: Color = Color::Rgb(0xea, 0xb6, 0x5d);

const RESOLVED: &str = include_str!("../../tuikit/themes-pi/resolved.json");

#[derive(Clone, Debug)]
pub struct PiTheme {
    pub name: String,
    pub light: bool,
    paints: Vec<Paint>,
}

fn parse_hex(s: &str) -> Option<Color> {
    let h = s.strip_prefix('#')?;
    let v = |i: usize, n: usize| u8::from_str_radix(&h[i..i + n], 16).ok();
    match h.len() {
        6 => Some(Color::Rgb(v(0, 2)?, v(2, 2)?, v(4, 2)?)),
        3 => {
            let d = |i: usize| v(i, 1).map(|x| x * 17);
            Some(Color::Rgb(d(0)?, d(1)?, d(2)?))
        }
        _ => None,
    }
}

impl PiTheme {
    pub fn builtin(name: &str) -> Option<PiTheme> {
        match name {
            "dark" | "light" => {
                let all: Value = serde_json::from_str(RESOLVED).ok()?;
                let t = all.get(name)?;
                let colors = t.get("colors")?;
                let paints = ALL_TOKENS
                    .iter()
                    .map(|k| {
                        colors
                            .get(k.name())
                            .and_then(Value::as_str)
                            .and_then(parse_hex)
                            .map_or(Paint::DEFAULT, |color| Paint {
                                color,
                                faint: false,
                            })
                    })
                    .collect();
                Some(PiTheme {
                    name: name.into(),
                    light: name == "light",
                    paints,
                })
            }
            "system" => Some(Self::system_fallback()),
            _ => None,
        }
    }

    pub fn dark() -> PiTheme {
        Self::builtin("dark").expect("resolved.json carries dark")
    }

    /// Pi's `system` theme when the terminal answers no colour query (tmux, most CI): ANSI palette
    /// indices, the terminal default for panels and body text, faint for neutral tokens.
    pub fn system_fallback() -> PiTheme {
        Self::system(&Reported::default(), Appearance::Dark)
    }

    /// Pi's `system` theme for what the terminal reported.
    pub fn system(reported: &Reported, hint: Appearance) -> PiTheme {
        Self::from_generated(&system_theme::generate(reported, 1.0, hint))
    }

    pub fn from_generated(g: &Generated) -> PiTheme {
        let paints = ALL_TOKENS
            .iter()
            .map(|&t| Paint {
                color: match g.colors.get(t.name()) {
                    Some(Gen::Rgb(c)) => {
                        let (r, g, b) = c.bytes();
                        Color::Rgb(r, g, b)
                    }
                    Some(Gen::Index(i)) => Color::Indexed(*i),
                    _ => Color::Reset,
                },
                faint: g.dim.contains(&t.name()),
            })
            .collect();
        PiTheme {
            name: "system".into(),
            light: g.appearance == Appearance::Light,
            paints,
        }
    }

    /// A user theme file. Values may be `#rgb`, `#rrggbb`, a palette index, a name from `vars`, or
    /// `""`. Tokens the file leaves out or writes in a form this loader cannot read come from
    /// `dark`.
    pub fn from_json(src: &str) -> Result<PiTheme, String> {
        let v: Value = serde_json::from_str(src).map_err(|e| e.to_string())?;
        let name = v
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("custom")
            .to_string();
        let light = v.get("appearance").and_then(Value::as_str) == Some("light");
        let vars = v.get("vars").cloned().unwrap_or(Value::Null);
        let colors = v.get("colors").ok_or("theme has no \"colors\"")?;
        let mut base = Self::builtin(if light { "light" } else { "dark" }).expect("builtin");
        fn resolve(x: &Value, vars: &Value, depth: u8) -> Option<Paint> {
            match x {
                Value::String(s) if s.is_empty() => Some(Paint::DEFAULT),
                Value::String(s) if s.starts_with('#') => parse_hex(s).map(|color| Paint {
                    color,
                    faint: false,
                }),
                Value::String(s) if depth < 4 => resolve(vars.get(s)?, vars, depth + 1),
                Value::Number(n) => n.as_u64().filter(|n| *n < 256).map(|n| Paint {
                    color: Color::Indexed(n as u8),
                    faint: false,
                }),
                _ => None,
            }
        }
        for (i, k) in ALL_TOKENS.iter().enumerate() {
            if let Some(p) = colors.get(k.name()).and_then(|x| resolve(x, &vars, 0)) {
                base.paints[i] = p;
            }
        }
        base.name = name;
        Ok(base)
    }

    pub fn paint(&self, t: Tok) -> Paint {
        self.paints[t as usize]
    }

    pub fn color(&self, t: Tok) -> Color {
        self.paint(t).color
    }

    /// Foreground style of a token.
    pub fn fg(&self, t: Tok) -> Style {
        let p = self.paint(t);
        let mut s = Style::default();
        if p.color != Color::Reset {
            s = s.fg(p.color);
        }
        if p.faint {
            s = s.add_modifier(Modifier::DIM);
        }
        s
    }

    /// Background style of a token; the terminal default paints nothing.
    pub fn bg(&self, t: Tok) -> Style {
        let p = self.paint(t);
        if p.color == Color::Reset {
            Style::default()
        } else {
            Style::default().bg(p.color)
        }
    }

    pub fn has_bg(&self, t: Tok) -> bool {
        self.paint(t).color != Color::Reset
    }

    pub fn bold(&self, t: Tok) -> Style {
        self.fg(t).add_modifier(Modifier::BOLD)
    }

    pub fn thinking(&self, level: &str) -> Tok {
        match level {
            "off" => Tok::ThinkingOff,
            "minimal" => Tok::ThinkingMinimal,
            "low" => Tok::ThinkingLow,
            "high" => Tok::ThinkingHigh,
            "xhigh" => Tok::ThinkingXhigh,
            "max" => Tok::ThinkingMax,
            // `medium`, wizard's `default`, and anything unknown
            _ => Tok::ThinkingMedium,
        }
    }
}

/// What the terminal has told us so far about its colours, and the theme setting that depends on it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TermColors {
    pub reported: Reported,
    /// The last light or dark report of mode 2031. Only read while no background is known.
    pub scheme: Option<Appearance>,
}

impl TermColors {
    /// `detectTerminalTheme`: the reported background decides; without one the terminal's own
    /// light/dark report, then `COLORFGBG`, then dark.
    pub fn appearance(&self) -> Appearance {
        self.appearance_with(std::env::var("COLORFGBG").ok().as_deref())
    }

    pub fn appearance_with(&self, colorfgbg: Option<&str>) -> Appearance {
        match self.reported.background {
            Some(bg) => system_theme::terminal_appearance(bg, self.reported.foreground),
            None => self
                .scheme
                .or_else(|| colorfgbg.and_then(colorfgbg_appearance))
                .unwrap_or(Appearance::Dark),
        }
    }
}

/// `detectColorFgBgTheme`: the last field is an ANSI index; 0 to 6 and 8 are dark, 7 and 9 to 15
/// light, anything else says nothing.
fn colorfgbg_appearance(v: &str) -> Option<Appearance> {
    let bg = v.rsplit(';').next()?.trim();
    if bg.is_empty() || bg.len() > 2 || !bg.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    match bg.parse::<u8>().ok()? {
        0..=6 | 8 => Some(Appearance::Dark),
        7 | 9..=15 => Some(Appearance::Light),
        _ => None,
    }
}

/// `resolveThemeSetting`: a `light/dark` pair picks by the terminal's appearance, a plain name is
/// itself, nothing set (or a malformed pair) is `None`, which means the `system` theme.
pub fn resolve_setting(setting: Option<&str>, appearance: Appearance) -> Option<String> {
    let s = setting?;
    if let Some((light, dark)) = s.split_once('/') {
        let (light, dark) = (light.trim(), dark.trim());
        if dark.contains('/') || light.is_empty() || dark.is_empty() {
            return None;
        }
        return Some(
            if appearance == Appearance::Light {
                light
            } else {
                dark
            }
            .to_string(),
        );
    }
    Some(s.to_string())
}

/// Pi's `queryTerminalColors` bookkeeping: OSC 10, OSC 11 and OSC 4 for the 16 palette slots,
/// followed by a DA1 request that marks the end of the replies. The batch is complete when the
/// DA1 reply or all 18 colour replies have arrived. Pure, so the pty test and the unit tests
/// drive the same code the app does.
#[derive(Debug, Default)]
pub struct ColorQuery {
    foreground: Option<Rgb>,
    background: Option<Rgb>,
    palette: [Option<Rgb>; 16],
    replied: HashSet<u16>,
    done: bool,
}

/// OSC 10, OSC 11, OSC 4 for slots 0 to 15, then DA1. Written raw, as Pi does: tmux 3.4 and later
/// forward and answer the colour queries themselves, and Pi wraps nothing for older ones.
pub const COLOR_QUERY: &str = concat!(
    "\x1b]10;?\x07\x1b]11;?\x07",
    "\x1b]4;0;?\x07\x1b]4;1;?\x07\x1b]4;2;?\x07\x1b]4;3;?\x07",
    "\x1b]4;4;?\x07\x1b]4;5;?\x07\x1b]4;6;?\x07\x1b]4;7;?\x07",
    "\x1b]4;8;?\x07\x1b]4;9;?\x07\x1b]4;10;?\x07\x1b]4;11;?\x07",
    "\x1b]4;12;?\x07\x1b]4;13;?\x07\x1b]4;14;?\x07\x1b]4;15;?\x07",
    "\x1b[c"
);

/// How long Pi waits for the batch before it builds the theme from what it has.
pub const COLOR_QUERY_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(100);

impl ColorQuery {
    pub fn new() -> ColorQuery {
        ColorQuery::default()
    }

    /// Feed a reply. Returns `true` when this one completes the batch.
    pub fn feed(&mut self, r: Reply) -> bool {
        if self.done {
            return false;
        }
        match r {
            Reply::DeviceAttributes => {
                self.done = true;
                return true;
            }
            Reply::Color { target, rgb } => {
                let key = match target {
                    ColorTarget::Foreground => 256,
                    ColorTarget::Background => 257,
                    ColorTarget::Palette(n) => n as u16,
                };
                if !self.replied.insert(key) {
                    return false;
                }
                let rgb = rgb.map(|(r, g, b)| Rgb::new(r, g, b));
                match target {
                    ColorTarget::Foreground => self.foreground = rgb,
                    ColorTarget::Background => self.background = rgb,
                    ColorTarget::Palette(n) if n < 16 => self.palette[n as usize] = rgb,
                    ColorTarget::Palette(_) => {}
                }
            }
        }
        if self.replied.len() == 18 {
            self.done = true;
        }
        self.done
    }

    pub fn done(&self) -> bool {
        self.done
    }

    /// What arrived. The palette counts only when all 16 colours did.
    pub fn reported(&self) -> Reported {
        let mut pal = [Rgb::new(0, 0, 0); 16];
        let full = self.palette.iter().enumerate().all(|(i, c)| match c {
            Some(c) => {
                pal[i] = *c;
                true
            }
            None => false,
        });
        Reported {
            foreground: self.foreground,
            background: self.background,
            palette: full.then_some(pal),
        }
    }
}

/// Fold a new report into the previous one the way Pi's `applyTerminalColors` does: a field the
/// terminal did not report this time keeps its last value.
pub fn merge_reported(previous: &Reported, new: &Reported) -> Reported {
    Reported {
        foreground: new.foreground.or(previous.foreground),
        background: new.background.or(previous.background),
        palette: new.palette.or(previous.palette),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dark_matches_the_spec_table() {
        let t = PiTheme::dark();
        assert_eq!(t.color(Tok::Accent), Color::Rgb(0xa7, 0x98, 0xd7));
        assert_eq!(t.color(Tok::ToolSuccessBg), Color::Rgb(0x25, 0x41, 0x31));
        assert_eq!(t.color(Tok::ThinkingMedium), Color::Rgb(0x61, 0x85, 0xcc));
    }

    #[test]
    fn light_and_every_token_resolve() {
        for n in ["dark", "light"] {
            let t = PiTheme::builtin(n).unwrap();
            for k in ALL_TOKENS {
                assert_ne!(t.color(*k), Color::Reset, "{n} {}", k.name());
            }
        }
    }

    #[test]
    fn system_fallback_is_faint_and_unpainted() {
        let t = PiTheme::system_fallback();
        assert_eq!(t.color(Tok::UserMessageBg), Color::Reset);
        assert!(t.paint(Tok::Muted).faint);
        assert_eq!(t.color(Tok::ThinkingMedium), Color::Indexed(6));
        assert!(!t.has_bg(Tok::ToolSuccessBg));
    }

    #[test]
    fn user_theme_reads_vars_and_falls_back() {
        let t = PiTheme::from_json(
            r##"{"name":"mine","vars":{"c":"#102030"},"colors":{"accent":"c","error":196,"muted":"okhsl(1 2% 3%)"}}"##,
        )
        .unwrap();
        assert_eq!(t.color(Tok::Accent), Color::Rgb(0x10, 0x20, 0x30));
        assert_eq!(t.color(Tok::Error), Color::Indexed(196));
        assert_eq!(t.color(Tok::Muted), PiTheme::dark().color(Tok::Muted));
    }
}
