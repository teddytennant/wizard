//! Semantic color tokens for the TUI.
//!
//! Nothing that draws should name a color. It names a *token* (`accent`,
//! `muted`, `error`, `tool.running`) and a [`Theme`] (a data file, not code)
//! says what that token looks like. Three things depend on this indirection:
//!
//! - **Low-color terminals.** [`ColorDepth`] degrades every token down to the
//!   16-color ANSI palette (or to `Reset` when the terminal wants no color at
//!   all). Windows ConHost lands here, and so does any `TERM` without a
//!   `256color` suffix. It is a real code path, exercised by tests, not a
//!   promise to handle it later.
//! - **The native GUI**, which wants visual continuity with the TUI: one token
//!   table, two renderers.
//!
//! There is nothing to *choose* here. The palette is whichever one the active
//! UI skin came with ([`crate::skin::Skin::companion_theme`]) — `wizard`
//! under the house skin, `minimal` when a name does not load — because a skin
//! owns its colors the same way it owns its frame. What this module still owns
//! is the token vocabulary the renderers ask in, and the degradation to what
//! the terminal can actually render.
//!
//! Color *depth* has its own order, and `NO_COLOR` is at the top of it: see
//! [`ColorDepth::from_env`].
//!
//! The palettes are embedded with `include_str!`, so a fresh install has all of
//! them with no data directory on disk.

use std::cell::RefCell;
use std::str::FromStr;
use std::sync::{Arc, OnceLock, PoisonError, RwLock};

use anyhow::{Context, Result, bail};
use ratatui::style::{Color, Style};
use ratatui::widgets::BorderType;

/// Theme used when nothing else is chosen.
pub const DEFAULT_THEME: &str = "minimal";

/// Environment variable forcing a color depth: `mono`, `16`, `256`,
/// `truecolor`, or `auto` to fall back to detection.
///
/// The numeric spellings follow the conventions people already have in their
/// shell profiles: `0` means off (`CLICOLOR=0`), `1` means on (`CLICOLOR=1`,
/// `FORCE_COLOR=1`) and is read as the 16-color floor every terminal can
/// render. `16`, `256` and `24bit`/`truecolor` name a depth outright.
///
/// This is Wizard's own knob, so it does **not** outrank `NO_COLOR`, which is
/// a cross-tool contract a user sets once for every program on the machine.
/// [`ColorDepth::from_env`] owns that order and states it in full.
pub const ENV_COLOR: &str = "WIZARD_COLOR";

const MINIMAL_TOML: &str = include_str!("../assets/themes/minimal.toml");
const WIZARD_TOML: &str = include_str!("../assets/themes/wizard.toml");
const CODEX_TOML: &str = include_str!("../assets/themes/codex.toml");
const GROK_TOML: &str = include_str!("../assets/themes/grok.toml");

/// The palettes compiled into the binary: one per UI skin, and nothing else.
///
/// There is no longer a theme *catalog* to pick from, because there is no
/// longer anything that picks. A skin owns its colors the same way it owns its
/// frame ([`crate::skin::Skin::companion_theme`]), so these are reachable by
/// name only so that a skin can name the one it came with.
const BUILTINS: [(&str, &str); 4] = [
    ("minimal", MINIMAL_TOML),
    ("wizard", WIZARD_TOML),
    ("codex", CODEX_TOML),
    ("grok", GROK_TOML),
];

// ---------------------------------------------------------------------------
// Tokens
// ---------------------------------------------------------------------------

/// A semantic color slot. Renderers ask for one of these; only theme data
/// ever names a literal color.
///
/// The discriminant order is load-bearing: [`Token::index`] is `self as usize`
/// into a theme's color array, and [`Token::ALL`] must list the variants in
/// declaration order (asserted by a test).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Token {
    /// Body text. `reset` in both built-ins so the terminal's own foreground
    /// shows through.
    Text,
    /// Secondary text: tool output, the user's echoed prompt, details.
    Muted,
    /// Dim chrome: rules, gutter marks, hints, list bullets.
    Faint,
    /// The one accent: prompt glyph, gutters, names, selection markers.
    Accent,
    /// Inline code and rendered math.
    Code,
    /// Markdown headings, and the focused question in the interview modal.
    Heading,
    /// The URL trailing a markdown link.
    Link,
    /// Block-quoted text.
    Quote,
    /// Box borders on floating layers.
    Border,
    /// Something went wrong and the user must read it.
    Error,
    /// Something is off but the turn continues (sovereign mode, a provider
    /// that did not answer its health probe).
    Warning,
    /// A step completed.
    Success,
    /// A tool call / subagent / session that is still working.
    ToolRunning,
    /// A tool call / subagent / session that finished cleanly.
    ToolDone,
    /// A tool call / subagent / session that failed.
    ToolFailed,
    /// An added line in the `/diff` sidebar.
    DiffAdd,
    /// A removed line in the `/diff` sidebar.
    DiffDel,
    /// A diff's file / index headers.
    DiffMeta,
    /// A diff's `@@` hunk headers.
    DiffHunk,
    /// The slab behind a block a skin *raises* off the background — Codex's
    /// user message, Grok Build's prompt band.
    ///
    /// A background, which is the one thing the rest of the palette never is,
    /// and the reason it is a token rather than a computed blend: a blend
    /// needs to know the terminal's own background, and asking the terminal
    /// means writing an escape query and blocking on the reply, which this
    /// codebase has already removed once for hanging the TUI at startup (see
    /// `app::term::setup_terminal`). Declaring it here means the slab renders
    /// on every terminal, and [`crate::skin::blend`] still adapts it when the
    /// background happens to be knowable from the environment.
    ///
    /// `reset` — the value both house themes give it — means "no slab", which
    /// is how the default look keeps its rule of never painting a background.
    BgRaised,
    /// The slab behind a block a skin *sinks* into the background: Grok
    /// Build's tool-output panels. `reset` means no slab.
    BgSunken,
    /// The page grok-build fills with `bg_base`. `reset` means do not fill,
    /// so empty cells stay the terminal's own background (the house look).
    BgBase,
}

impl Token {
    /// Every token, in discriminant order.
    pub const ALL: [Token; 22] = [
        Token::Text,
        Token::Muted,
        Token::Faint,
        Token::Accent,
        Token::Code,
        Token::Heading,
        Token::Link,
        Token::Quote,
        Token::Border,
        Token::Error,
        Token::Warning,
        Token::Success,
        Token::ToolRunning,
        Token::ToolDone,
        Token::ToolFailed,
        Token::DiffAdd,
        Token::DiffDel,
        Token::DiffMeta,
        Token::DiffHunk,
        Token::BgRaised,
        Token::BgSunken,
        Token::BgBase,
    ];

    /// The key this token has in a theme file.
    pub fn key(self) -> &'static str {
        match self {
            Token::Text => "text",
            Token::Muted => "muted",
            Token::Faint => "faint",
            Token::Accent => "accent",
            Token::Code => "code",
            Token::Heading => "heading",
            Token::Link => "link",
            Token::Quote => "quote",
            Token::Border => "border",
            Token::Error => "error",
            Token::Warning => "warning",
            Token::Success => "success",
            Token::ToolRunning => "tool.running",
            Token::ToolDone => "tool.done",
            Token::ToolFailed => "tool.failed",
            Token::DiffAdd => "diff.add",
            Token::DiffDel => "diff.del",
            Token::DiffMeta => "diff.meta",
            Token::DiffHunk => "diff.hunk",
            Token::BgRaised => "bg.raised",
            Token::BgSunken => "bg.sunken",
            Token::BgBase => "bg.base",
        }
    }

    /// The token a theme file's key names, if any.
    pub fn from_key(key: &str) -> Option<Token> {
        Token::ALL.into_iter().find(|token| token.key() == key)
    }

    fn index(self) -> usize {
        self as usize
    }
}

// ---------------------------------------------------------------------------
// Color depth
// ---------------------------------------------------------------------------

/// What the terminal can actually render. Themes are authored at full depth
/// and degraded into this on load, so a 16-color terminal never receives an
/// escape sequence it will print as garbage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ColorDepth {
    /// No color at all (`NO_COLOR`, `TERM=dumb`). Every token becomes
    /// `Reset`; emphasis still reads through bold/italic/glyphs, which is why
    /// the UI never encodes meaning in color alone.
    Mono,
    /// The 16 ANSI colors and nothing else. Windows ConHost, `TERM=xterm`,
    /// most serial and embedded terminals.
    Ansi16,
    /// The xterm 256-color cube.
    Ansi256,
    /// 24-bit RGB.
    TrueColor,
}

impl ColorDepth {
    /// Read the depth from the process environment.
    pub fn detect() -> ColorDepth {
        let term = std::env::var("TERM").ok();
        let depth = Self::from_env(
            std::env::var(ENV_COLOR).ok().as_deref(),
            std::env::var("NO_COLOR").ok().as_deref(),
            std::env::var("COLORTERM").ok().as_deref(),
            term.as_deref(),
        );
        // Inside tmux or screen, COLORTERM is often the outer shell's and the
        // pane is 256. [`crate::mux::degrade`] keeps truecolor only when the
        // multiplexer advertises RGB.
        crate::mux::degrade(depth, term.as_deref())
    }

    /// Testable core of [`detect`](Self::detect).
    ///
    /// Precedence, highest first, and a test asserts each step against the one
    /// below it:
    ///
    /// 1. **`NO_COLOR`** (set and non-empty) is [`Mono`], and nothing overrides
    ///    it. It is a cross-tool contract: a user sets it once, in a profile,
    ///    to mean "no program on this machine paints my terminal". Letting
    ///    Wizard's own variable win over it would make Wizard the one program
    ///    that ignores the setting, which is exactly what the convention
    ///    exists to prevent. Someone who wants color from Wizard in a
    ///    `NO_COLOR` shell unsets it for the one command (`NO_COLOR= wizard`):
    ///    an empty value is "not set", per the same convention.
    /// 2. **`WIZARD_COLOR`**, the escape hatch for a terminal that lies about
    ///    itself in either direction. Unrecognised values (and `auto`) fall
    ///    through rather than forcing anything.
    /// 3. **`TERM=dumb`**, then the usual `COLORTERM` / `TERM` sniffing. An
    ///    absent `TERM` is treated as 16 colors rather than as truecolor:
    ///    that is the conservative guess, and it is what Windows ConHost looks
    ///    like.
    ///
    /// [`Mono`]: ColorDepth::Mono
    pub fn from_env(
        wizard_color: Option<&str>,
        no_color: Option<&str>,
        colorterm: Option<&str>,
        term: Option<&str>,
    ) -> ColorDepth {
        if no_color.is_some_and(|value| !value.is_empty()) {
            return ColorDepth::Mono;
        }
        if let Some(forced) = wizard_color.and_then(Self::parse) {
            return forced;
        }
        let term = term.unwrap_or_default();
        if term == "dumb" {
            return ColorDepth::Mono;
        }
        let colorterm = colorterm.unwrap_or_default().to_ascii_lowercase();
        if colorterm.contains("truecolor") || colorterm.contains("24bit") {
            return ColorDepth::TrueColor;
        }
        if term.contains("256color") || term.contains("direct") {
            return ColorDepth::Ansi256;
        }
        ColorDepth::Ansi16
    }

    /// How this depth reads in the UI (`/theme` reports it, because "my colors
    /// look wrong" is nearly always this value being lower than expected).
    pub fn label(self) -> &'static str {
        match self {
            ColorDepth::Mono => "no color",
            ColorDepth::Ansi16 => "16 colors",
            ColorDepth::Ansi256 => "256 colors",
            ColorDepth::TrueColor => "truecolor",
        }
    }

    /// Parse an explicit depth request (`WIZARD_COLOR`). `auto` and anything
    /// unrecognised return `None`, meaning "keep detecting".
    ///
    /// `1` is `on`, not `off`. It is the one genuinely ambiguous spelling
    /// here, because every other value in the table is a color *count* and
    /// "one color" could be read as [`Mono`]; the tie goes to the convention
    /// people already have in their profiles (`CLICOLOR=1`, `FORCE_COLOR=1`,
    /// `NO_COLOR` for the other direction), because that is what a user
    /// typing it is copying. Reading it as [`Mono`] handed a user who set the
    /// variable to *force* color on a completely uncolored UI instead.
    ///
    /// Turning color off has three unambiguous spellings that all still work
    /// (`0`, `off`/`none`, `mono`), plus `NO_COLOR`, which outranks this
    /// variable entirely, so nothing is lost by giving `1` to the convention.
    ///
    /// [`Mono`]: ColorDepth::Mono
    fn parse(value: &str) -> Option<ColorDepth> {
        match value.trim().to_ascii_lowercase().as_str() {
            "mono" | "none" | "off" | "0" => Some(ColorDepth::Mono),
            "16" | "ansi" | "ansi16" | "basic" | "on" | "1" => Some(ColorDepth::Ansi16),
            "256" | "ansi256" | "8bit" => Some(ColorDepth::Ansi256),
            "truecolor" | "24bit" | "16m" | "rgb" => Some(ColorDepth::TrueColor),
            _ => None,
        }
    }

    /// Bring `color` into this depth. Colors already inside the palette pass
    /// through untouched, so degrading twice is idempotent.
    pub fn adapt(self, color: Color) -> Color {
        match self {
            ColorDepth::TrueColor => color,
            ColorDepth::Ansi256 => match color {
                Color::Rgb(r, g, b) => Color::Indexed(nearest_xterm256(r, g, b)),
                other => other,
            },
            ColorDepth::Ansi16 => to_ansi16(color),
            ColorDepth::Mono => Color::Reset,
        }
    }
}

/// The 16 ANSI colors as ratatui names, paired with the RGB values xterm
/// gives them. Nearest-neighbor matching against this table is how any
/// deeper color finds its 16-color stand-in.
const ANSI16: [(Color, (u8, u8, u8)); 16] = [
    (Color::Black, (0, 0, 0)),
    (Color::Red, (128, 0, 0)),
    (Color::Green, (0, 128, 0)),
    (Color::Yellow, (128, 128, 0)),
    (Color::Blue, (0, 0, 128)),
    (Color::Magenta, (128, 0, 128)),
    (Color::Cyan, (0, 128, 128)),
    (Color::Gray, (192, 192, 192)),
    (Color::DarkGray, (128, 128, 128)),
    (Color::LightRed, (255, 0, 0)),
    (Color::LightGreen, (0, 255, 0)),
    (Color::LightYellow, (255, 255, 0)),
    (Color::LightBlue, (0, 0, 255)),
    (Color::LightMagenta, (255, 0, 255)),
    (Color::LightCyan, (0, 255, 255)),
    (Color::White, (255, 255, 255)),
];

/// Is this color already inside the 16-color palette (or `Reset`)? The
/// low-color fallback's postcondition, and what its test asserts.
pub fn is_ansi16(color: Color) -> bool {
    matches!(color, Color::Reset) || ANSI16.iter().any(|(named, _)| *named == color)
}

/// Collapse any color onto the 16-color palette.
fn to_ansi16(color: Color) -> Color {
    match color {
        Color::Reset => Color::Reset,
        Color::Rgb(r, g, b) => nearest_ansi16(r, g, b),
        Color::Indexed(index) => {
            let (r, g, b) = xterm256_rgb(index);
            nearest_ansi16(r, g, b)
        }
        named => named,
    }
}

/// The closest of the 16 ANSI colors to an RGB triple, by squared distance.
fn nearest_ansi16(r: u8, g: u8, b: u8) -> Color {
    ANSI16
        .iter()
        .min_by_key(|(_, rgb)| rgb_distance(*rgb, (r, g, b)))
        .map(|(named, _)| *named)
        .unwrap_or(Color::Reset)
}

/// The closest xterm-256 index to an RGB triple. The search covers the color
/// cube *and* the gray ramp (so near-grays land on the ramp instead of a muddy
/// cube cell) but skips indices 0-15: those sixteen are whatever the user's
/// terminal profile decided they are, while 16-255 are fixed, and a color
/// asked for in RGB should not be re-mapped through someone's Solarized.
fn nearest_xterm256(r: u8, g: u8, b: u8) -> u8 {
    (16u8..=255)
        .min_by_key(|index| rgb_distance(xterm256_rgb(*index), (r, g, b)))
        .unwrap_or(16)
}

fn rgb_distance(a: (u8, u8, u8), b: (u8, u8, u8)) -> u32 {
    let d = |x: u8, y: u8| {
        let diff = i32::from(x) - i32::from(y);
        (diff * diff) as u32
    };
    d(a.0, b.0) + d(a.1, b.1) + d(a.2, b.2)
}

/// RGB for an xterm-256 palette index: the 16 base colors, then the 6×6×6
/// color cube, then the 24-step gray ramp.
fn xterm256_rgb(index: u8) -> (u8, u8, u8) {
    match index {
        0..=15 => ANSI16[index as usize].1,
        16..=231 => {
            const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
            let n = u32::from(index) - 16;
            (
                LEVELS[(n / 36) as usize],
                LEVELS[((n / 6) % 6) as usize],
                LEVELS[(n % 6) as usize],
            )
        }
        _ => {
            let level = 8 + (u32::from(index) - 232) * 10;
            let level = level as u8;
            (level, level, level)
        }
    }
}

// ---------------------------------------------------------------------------
// Theme
// ---------------------------------------------------------------------------

/// A named token table plus the little chrome it also owns.
///
/// `declared` is what the theme file said; `resolved` is that table brought
/// into [`Theme::depth`]. Keeping both is what lets the depth change later
/// (the terminal is probed after the theme loads) without re-reading the file.
#[derive(Debug, Clone)]
pub struct Theme {
    pub name: String,
    pub description: String,
    /// Border symbols for floating layers. Sectioning chrome is borderless in
    /// every theme (padding and dim rules do that job); this is only the
    /// popups, which overlay live text and need an edge.
    pub border: BorderType,
    depth: ColorDepth,
    declared: [Color; Token::ALL.len()],
    resolved: [Color; Token::ALL.len()],
}

impl Theme {
    /// The color for `token`, already adapted to this theme's depth.
    pub fn color(&self, token: Token) -> Color {
        self.resolved[token.index()]
    }

    /// A foreground-only style for `token`. Nothing here ever sets a
    /// background: the TUI renders on the terminal's own, and an opaque slab
    /// would break that.
    pub fn style(&self, token: Token) -> Style {
        Style::default().fg(self.color(token))
    }

    /// The color as the theme file wrote it, before depth adaptation.
    pub fn declared(&self, token: Token) -> Color {
        self.declared[token.index()]
    }

    pub fn depth(&self) -> ColorDepth {
        self.depth
    }

    /// Bring an externally computed color into this theme's depth. The
    /// syntax highlighter builds its own grays, and they must degrade with
    /// everything else or a 16-color terminal gets RGB escapes for code
    /// blocks alone.
    pub fn adapt(&self, color: Color) -> Color {
        self.depth.adapt(color)
    }

    /// The same theme rendered for a different terminal.
    pub fn with_depth(&self, depth: ColorDepth) -> Theme {
        let mut resolved = self.declared;
        for slot in &mut resolved {
            *slot = depth.adapt(*slot);
        }
        Theme {
            name: self.name.clone(),
            description: self.description.clone(),
            border: self.border,
            depth,
            declared: self.declared,
            resolved,
        }
    }

    /// Parse a theme file. Tokens it does not name inherit from `defaults`,
    /// so a user theme can be three lines; unknown keys are an error, because
    /// a silently ignored typo is a theme that looks broken for no reason.
    ///
    /// "Unknown keys" means both levels. Only misspellings inside `[tokens]`
    /// used to be caught, so `Border = "double"` (capitalized), `boarder`, or
    /// `discription` parsed cleanly, loaded without a warning and did nothing
    /// at all, which is the exact failure the token-key check exists to
    /// prevent, one level up.
    pub fn parse(name: &str, source: &str, defaults: &Theme) -> Result<Theme> {
        /// Every key a theme file may carry at the top level.
        const TOP_LEVEL_KEYS: [&str; 4] = ["name", "description", "border", "tokens"];

        let value: toml::Value =
            toml::from_str(source).with_context(|| format!("parsing theme '{name}'"))?;
        let table = value
            .as_table()
            .with_context(|| format!("theme '{name}' must be a TOML table"))?;

        if let Some(unknown) = table
            .keys()
            .find(|key| !TOP_LEVEL_KEYS.contains(&key.as_str()))
        {
            bail!(
                "theme '{name}': unknown key '{unknown}' (expected one of {}, or a token under \
                 [tokens]; see assets/themes/minimal.toml)",
                TOP_LEVEL_KEYS.join(", ")
            );
        }

        let string = |key: &str| table.get(key).and_then(toml::Value::as_str);
        let border = match string("border") {
            Some(value) => parse_border(value)
                .with_context(|| format!("theme '{name}' has an unknown border '{value}'"))?,
            None => defaults.border,
        };

        let mut declared = defaults.declared;
        if let Some(tokens) = table.get("tokens") {
            let tokens = tokens
                .as_table()
                .with_context(|| format!("theme '{name}': [tokens] must be a table"))?;
            // Flattened so `diff.add = "green"` and `"diff.add" = "green"`
            // mean the same thing: TOML reads the first as a nested table, and
            // a user should not have to know that.
            let mut flat = Vec::new();
            flatten(String::new(), tokens, &mut flat);
            for (key, value) in flat {
                let token = Token::from_key(&key).with_context(|| {
                    format!(
                        "theme '{name}': unknown token '{key}' (see assets/themes/minimal.toml)"
                    )
                })?;
                declared[token.index()] = parse_color(&value)
                    .with_context(|| format!("theme '{name}': token '{key}' is not a color"))?;
            }
        }

        let theme = Theme {
            name: string("name").unwrap_or(name).to_string(),
            description: string("description").unwrap_or_default().to_string(),
            border,
            depth: ColorDepth::TrueColor,
            declared,
            resolved: declared,
        };
        Ok(theme.with_depth(defaults.depth))
    }
}

/// The base every theme is layered on: no color anywhere, plain borders.
/// `minimal` names every token (a test enforces it), so this is only ever
/// visible through a user theme that leaves one out.
fn bare() -> Theme {
    Theme {
        name: "bare".to_string(),
        description: String::new(),
        border: BorderType::Plain,
        depth: ColorDepth::TrueColor,
        declared: [Color::Reset; Token::ALL.len()],
        resolved: [Color::Reset; Token::ALL.len()],
    }
}

/// The built-in default, parsed once. A failure here is a bug in a file that
/// ships inside the binary, so it degrades to [`bare`] rather than killing the
/// TUI; the test suite is what keeps that from happening silently.
pub fn minimal() -> Arc<Theme> {
    static MINIMAL: OnceLock<Arc<Theme>> = OnceLock::new();
    MINIMAL
        .get_or_init(|| {
            Arc::new(Theme::parse(DEFAULT_THEME, MINIMAL_TOML, &bare()).unwrap_or_else(|_| bare()))
        })
        .clone()
}

fn parse_border(value: &str) -> Option<BorderType> {
    match value.trim().to_ascii_lowercase().as_str() {
        "plain" | "square" => Some(BorderType::Plain),
        "rounded" => Some(BorderType::Rounded),
        "double" => Some(BorderType::Double),
        "thick" => Some(BorderType::Thick),
        _ => None,
    }
}

/// A token value: a color name, an `#rrggbb` string, a palette index as a
/// string, or a bare TOML integer (`muted = 246`, which is what people write).
fn parse_color(value: &toml::Value) -> Result<Color> {
    match value {
        toml::Value::String(text) => Color::from_str(text)
            .map_err(|_| anyhow::anyhow!("'{text}' is not a color name, #rrggbb, or 0-255")),
        toml::Value::Integer(index) => u8::try_from(*index)
            .map(Color::Indexed)
            .map_err(|_| anyhow::anyhow!("palette index {index} is outside 0-255")),
        other => bail!("expected a color, found {}", other.type_str()),
    }
}

/// Flatten nested TOML tables into dotted keys (`diff` → `add` becomes
/// `diff.add`).
fn flatten(prefix: String, table: &toml::value::Table, out: &mut Vec<(String, toml::Value)>) {
    for (key, value) in table {
        let path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        match value {
            toml::Value::Table(nested) => flatten(path, nested, out),
            other => out.push((path, other.clone())),
        }
    }
}

// ---------------------------------------------------------------------------
// Loading and resolution
// ---------------------------------------------------------------------------

/// Load a skin's companion palette by name.
///
/// This used to consult `~/.wizard/themes/<name>.toml` before the embedded
/// copies, so a user could ship their own palette or shadow a built-in. That
/// went with the rest of the selection layer: a file that can only be reached
/// by a setting nothing reads any more is not an extension point, it is a
/// directory that silently stops working.
pub fn load(name: &str) -> Result<Theme> {
    let defaults = minimal();
    match BUILTINS.iter().find(|(builtin, _)| *builtin == name) {
        Some((_, source)) => Theme::parse(name, source, &defaults),
        None => bail!(
            "unknown palette '{name}' (built in: {})",
            BUILTINS
                .iter()
                .map(|(builtin, _)| *builtin)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Install the active palette at process start: load the one the active skin
/// came with and degrade it to whatever the terminal can render.
///
/// `name` is the active skin's companion palette
/// ([`crate::skin::Skin::companion_theme`]). It used to be the bottom of a
/// three-level resolution order — `[ui] theme`, then `WIZARD_THEME`, then
/// this — which existed so a palette could be chosen independently of the
/// chrome around it. Skins made that redundant: a skin that draws Codex's
/// frame in someone else's colors is not the thing anyone asked for, and the
/// order's real effect was that picking a skin quietly did nothing to the
/// colors for any user who had ever set a theme. Call this *after*
/// [`crate::skin::init`] so it is the skin the user actually has.
///
/// Returns a warning to show the user when the named theme could not be
/// loaded; the default is installed in that case, because a typo in a config
/// file must not cost anyone their TUI.
///
/// Installs through [`set_global`], deliberately, and *not* [`set_active`]:
/// `App::new` calls this on every construction, so writing through a thread's
/// pin would let a freshly built `App` silently replace the theme a second
/// renderer (the GUI, a test rendering a known palette) had pinned for itself.
/// [`active`] already prefers the pin, so a pinned thread never sees what this
/// installs.
pub fn init(name: &str) -> Option<String> {
    let (theme, warning) = init_theme(name, ColorDepth::detect());
    set_global(theme);
    warning
}

/// Testable core of [`init`]: resolve the name, load it, and degrade it to
/// `depth`. Returns the theme to install and the warning to show, and installs
/// nothing itself.
///
/// Installing is what made this untestable. The whole suite writes the
/// process-wide theme (every `App::new` does), so a test that called `init`
/// and then read [`active`] was asserting against a slot other threads were
/// writing between the two lines, and failed on timing rather than on
/// behaviour. With the decision separated from the installation, the
/// resolution chain is asserted on a value.
fn init_theme(name: &str, depth: ColorDepth) -> (Arc<Theme>, Option<String>) {
    let name = if name.trim().is_empty() {
        DEFAULT_THEME
    } else {
        name.trim()
    };
    match load(name) {
        Ok(theme) => (Arc::new(theme.with_depth(depth)), None),
        Err(err) => (
            Arc::new(minimal().with_depth(depth)),
            Some(format!("theme: {err:#}; using {DEFAULT_THEME}")),
        ),
    }
}

// ---------------------------------------------------------------------------
// The active theme
// ---------------------------------------------------------------------------

fn global() -> &'static RwLock<Arc<Theme>> {
    static ACTIVE: OnceLock<RwLock<Arc<Theme>>> = OnceLock::new();
    ACTIVE.get_or_init(|| RwLock::new(minimal()))
}

thread_local! {
    /// A theme pinned to this thread, which wins over the process-wide one.
    /// Tests use it to render against a known theme without disturbing any
    /// other thread; a second renderer (the GUI) can use it the same way.
    static PINNED: RefCell<Option<Arc<Theme>>> = const { RefCell::new(None) };
}

fn set_global(theme: Arc<Theme>) {
    *global().write().unwrap_or_else(PoisonError::into_inner) = theme;
}

/// The theme in force on this thread.
pub fn active() -> Arc<Theme> {
    if let Some(theme) = PINNED.with(|pinned| pinned.borrow().clone()) {
        return theme;
    }
    global()
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

/// Swap the active theme. A thread that has pinned one keeps its pin (the
/// swap lands there); otherwise this changes the theme process-wide.
pub fn set_active(theme: Arc<Theme>) {
    let pinned = PINNED.with(|pinned| {
        let mut slot = pinned.borrow_mut();
        if slot.is_some() {
            *slot = Some(theme.clone());
            true
        } else {
            false
        }
    });
    if !pinned {
        set_global(theme);
    }
}

/// Swap to a named theme, keeping the color depth already in force (the
/// terminal did not change just because the palette did).
pub fn set_active_by_name(name: &str) -> Result<Arc<Theme>> {
    let depth = active().depth();
    let theme = Arc::new(load(name)?.with_depth(depth));
    set_active(theme.clone());
    Ok(theme)
}

/// Re-render the active theme for a different terminal. This is the hook the
/// Windows console path uses once it knows ConHost cannot do better than 16
/// colors.
pub fn set_color_depth(depth: ColorDepth) {
    set_active(Arc::new(active().with_depth(depth)));
}

/// Pin `theme` to the current thread until the returned guard drops.
pub fn pin(theme: Arc<Theme>) -> Pinned {
    let previous = PINNED.with(|pinned| pinned.borrow_mut().replace(theme));
    Pinned { previous }
}

/// Guard returned by [`pin`]; restores the previous pin on drop.
pub struct Pinned {
    previous: Option<Arc<Theme>>,
}

impl Drop for Pinned {
    fn drop(&mut self) {
        let previous = self.previous.take();
        PINNED.with(|pinned| *pinned.borrow_mut() = previous);
    }
}

/// The active theme's color for `token`.
pub fn color(token: Token) -> Color {
    active().color(token)
}

/// The active theme's foreground style for `token`. This is what renderers
/// call.
pub fn style(token: Token) -> Style {
    active().style(token)
}

/// Bring an externally computed color into the active theme's depth.
pub fn adapt(color: Color) -> Color {
    active().adapt(color)
}

/// Border symbols for floating layers under the active theme.
pub fn border_type() -> BorderType {
    active().border
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_all_is_in_discriminant_order() {
        // `Token::index` is the discriminant, and every color array is
        // indexed by it: if ALL ever drifts from the declaration order, every
        // theme silently paints the wrong tokens.
        for (position, token) in Token::ALL.into_iter().enumerate() {
            assert_eq!(token.index(), position, "{token:?} is out of order");
        }
    }

    #[test]
    fn every_token_has_a_unique_key_that_round_trips() {
        let mut keys: Vec<&str> = Token::ALL.into_iter().map(Token::key).collect();
        keys.sort_unstable();
        let unique = {
            let mut copy = keys.clone();
            copy.dedup();
            copy
        };
        assert_eq!(keys, unique, "two tokens share a theme key");
        for token in Token::ALL {
            assert_eq!(Token::from_key(token.key()), Some(token));
        }
    }

    #[test]
    fn every_builtin_theme_parses_and_names_every_token() {
        for (name, source) in BUILTINS {
            let theme = load(name).unwrap_or_else(|err| panic!("{name} failed to load: {err:#}"));
            assert_eq!(theme.name, name);
            assert!(!theme.description.is_empty(), "{name} has no description");
            // The built-ins must be *complete* tables: a user theme inherits
            // whatever it leaves out from `minimal`, so a token missing from a
            // built-in would silently become `Reset` for everyone downstream.
            let declared = declared_keys(source);
            for token in Token::ALL {
                assert!(
                    declared.iter().any(|key| key == token.key()),
                    "{name} does not name '{}'",
                    token.key()
                );
            }
            for key in &declared {
                assert!(
                    Token::from_key(key).is_some(),
                    "{name} names an unknown token '{key}'"
                );
            }
        }
    }

    /// The token keys a theme file names, dotted.
    fn declared_keys(source: &str) -> Vec<String> {
        let value: toml::Value = toml::from_str(source).expect("theme parses");
        let tokens = value
            .get("tokens")
            .and_then(toml::Value::as_table)
            .expect("[tokens] table");
        let mut flat = Vec::new();
        flatten(String::new(), tokens, &mut flat);
        flat.into_iter().map(|(key, _)| key).collect()
    }

    #[test]
    fn the_default_theme_stays_monochrome() {
        // The charter's default look: grays plus one accent, no hues. It is
        // also what keeps `minimal` legible on a 16-color terminal without
        // any degradation at all.
        let theme = load("minimal").expect("minimal loads");
        for token in Token::ALL {
            let color = theme.declared(token);
            assert!(
                matches!(
                    color,
                    Color::Reset
                        | Color::White
                        | Color::Gray
                        | Color::DarkGray
                        | Color::Black
                        | Color::Green
                        | Color::Red
                ),
                "minimal token '{}' is {color:?}, which is not monochrome \
                 (green/red are allowed only for diffs and pass/fail)",
                token.key()
            );
        }
    }

    #[test]
    fn low_color_fallback_emits_only_sixteen_color_values() {
        for (name, _) in BUILTINS {
            let theme = load(name).expect("loads").with_depth(ColorDepth::Ansi16);
            for token in Token::ALL {
                let color = theme.color(token);
                assert!(
                    is_ansi16(color),
                    "{name} token '{}' degraded to {color:?}, which is not a 16-color value",
                    token.key()
                );
            }
            // The syntax highlighter's own grays go through the same door.
            assert!(is_ansi16(theme.adapt(Color::Rgb(180, 180, 180))));
            assert!(is_ansi16(theme.adapt(Color::Indexed(200))));
        }
    }

    #[test]
    fn mono_depth_drops_every_color() {
        let theme = load("codex").expect("loads").with_depth(ColorDepth::Mono);
        for token in Token::ALL {
            assert_eq!(theme.color(token), Color::Reset, "{}", token.key());
        }
        assert_eq!(theme.adapt(Color::Rgb(1, 2, 3)), Color::Reset);
    }

    #[test]
    fn degrading_is_idempotent_and_keeps_the_declared_table() {
        let theme = load("codex").expect("loads");
        let once = theme.with_depth(ColorDepth::Ansi16);
        let twice = once.with_depth(ColorDepth::Ansi16);
        for token in Token::ALL {
            assert_eq!(once.color(token), twice.color(token));
            // Declared values survive, so a later depth change can widen again.
            assert_eq!(once.declared(token), theme.declared(token));
        }
        let widened = once.with_depth(ColorDepth::TrueColor);
        for token in Token::ALL {
            assert_eq!(widened.color(token), theme.declared(token));
        }
    }

    #[test]
    fn nearest_sixteen_color_picks_the_obvious_neighbor() {
        assert_eq!(to_ansi16(Color::Rgb(250, 12, 8)), Color::LightRed);
        assert_eq!(to_ansi16(Color::Rgb(0, 0, 0)), Color::Black);
        assert_eq!(to_ansi16(Color::Rgb(255, 255, 255)), Color::White);
        // Palette index 231 is the cube's white corner; 16 is its black one.
        assert_eq!(to_ansi16(Color::Indexed(231)), Color::White);
        assert_eq!(to_ansi16(Color::Indexed(16)), Color::Black);
        // Already-16 values and Reset pass through untouched.
        assert_eq!(to_ansi16(Color::Cyan), Color::Cyan);
        assert_eq!(to_ansi16(Color::Reset), Color::Reset);
    }

    #[test]
    fn true_color_degrades_into_the_256_cube() {
        let depth = ColorDepth::Ansi256;
        assert_eq!(depth.adapt(Color::Rgb(0, 0, 0)), Color::Indexed(16));
        assert_eq!(depth.adapt(Color::Rgb(255, 255, 255)), Color::Indexed(231));
        // Named colors are already in the palette.
        assert_eq!(depth.adapt(Color::Magenta), Color::Magenta);
    }

    #[test]
    fn a_skin_gets_the_palette_it_came_with() {
        // The whole of the selection order now: whatever the skin names.
        let (theme, warning) = init_theme(
            crate::skin::Skin::Codex.companion_theme(),
            ColorDepth::TrueColor,
        );
        assert_eq!(theme.name, "codex");
        assert!(warning.is_none(), "{warning:?}");
    }

    #[test]
    fn a_blank_palette_name_still_lands_on_the_default() {
        let (theme, warning) = init_theme("", ColorDepth::TrueColor);
        assert_eq!(theme.name, DEFAULT_THEME);
        assert!(warning.is_none(), "{warning:?}");
    }

    #[test]
    fn an_unknown_palette_warns_and_falls_back_rather_than_costing_the_tui() {
        let (theme, warning) = init_theme("ember", ColorDepth::TrueColor);
        assert_eq!(
            theme.name, DEFAULT_THEME,
            "a palette that no longer ships must not leave the UI unrendered"
        );
        assert!(
            warning.is_some_and(|warning| warning.contains("ember")),
            "and the user has to be told which name went missing"
        );
    }

    #[test]
    fn color_depth_detection_prefers_no_color_then_the_override() {
        use ColorDepth::*;
        assert_eq!(
            ColorDepth::from_env(Some("16"), None, Some("truecolor"), Some("xterm-256color")),
            Ansi16
        );
        assert_eq!(ColorDepth::from_env(Some("mono"), None, None, None), Mono);
        assert_eq!(
            ColorDepth::from_env(None, Some("1"), Some("truecolor"), None),
            Mono
        );
        // An empty NO_COLOR is not set, per the convention.
        assert_eq!(
            ColorDepth::from_env(None, Some(""), Some("truecolor"), None),
            TrueColor
        );
        assert_eq!(
            ColorDepth::from_env(None, None, None, Some("xterm-256color")),
            Ansi256
        );
        assert_eq!(
            ColorDepth::from_env(None, None, None, Some("xterm")),
            Ansi16
        );
        assert_eq!(ColorDepth::from_env(None, None, None, Some("dumb")), Mono);
        // No TERM at all: the Windows console case, which gets 16 colors.
        assert_eq!(ColorDepth::from_env(None, None, None, None), Ansi16);
        // `auto` falls through to detection instead of forcing anything.
        assert_eq!(
            ColorDepth::from_env(Some("auto"), None, None, Some("xterm-256color")),
            Ansi256
        );
        // The numeric spellings follow CLICOLOR/FORCE_COLOR: 0 is off, 1 is
        // on. `WIZARD_COLOR=1` used to mean Mono, so a user forcing color on
        // got a UI with no color at all.
        assert_eq!(ColorDepth::from_env(Some("0"), None, None, None), Mono);
        assert_eq!(
            ColorDepth::from_env(Some("1"), None, Some("truecolor"), None),
            Ansi16,
            "1 means color on, at the depth every terminal can render"
        );
        assert_eq!(ColorDepth::from_env(Some("on"), None, None, None), Ansi16);
    }

    /// Adversarial: `NO_COLOR` is a contract with every program on the
    /// machine, not a hint Wizard's own variable may overrule. `WIZARD_COLOR`
    /// used to be read first, so a user with `NO_COLOR` in their profile and
    /// `WIZARD_COLOR=1` anywhere in their environment got a colored TUI.
    #[test]
    fn no_color_outranks_the_wizard_color_override() {
        use ColorDepth::*;
        for forced in ["1", "on", "16", "256", "truecolor", "24bit"] {
            assert_eq!(
                ColorDepth::from_env(
                    Some(forced),
                    Some("1"),
                    Some("truecolor"),
                    Some("xterm-256color")
                ),
                Mono,
                "NO_COLOR must win over WIZARD_COLOR={forced}"
            );
        }
        // Both pointing the same way is still Mono, and an unrecognised
        // override does not smuggle color past NO_COLOR either.
        assert_eq!(ColorDepth::from_env(Some("0"), Some("1"), None, None), Mono);
        assert_eq!(
            ColorDepth::from_env(Some("auto"), Some("yes"), Some("truecolor"), None),
            Mono
        );
        // The escape hatch stays open where the contract was never signed:
        // `NO_COLOR=` is "not set", so the override is back in force.
        assert_eq!(
            ColorDepth::from_env(Some("truecolor"), Some(""), None, Some("dumb")),
            TrueColor,
            "an empty NO_COLOR is absent, so WIZARD_COLOR still decides"
        );
    }

    #[test]
    fn a_user_theme_inherits_the_tokens_it_leaves_out() {
        let source = r##"
            description = "three lines"
            border = "double"
            [tokens]
            accent = "#ff00aa"
        "##;
        let theme = Theme::parse("mine", source, &minimal()).expect("parses");
        assert_eq!(theme.declared(Token::Accent), Color::Rgb(255, 0, 170));
        assert_eq!(
            theme.declared(Token::Muted),
            minimal().declared(Token::Muted)
        );
        assert_eq!(theme.border, BorderType::Double);
    }

    #[test]
    fn dotted_token_keys_parse_written_either_way() {
        let nested = Theme::parse(
            "nested",
            "[tokens]\n[tokens.diff]\nadd = \"blue\"\n",
            &minimal(),
        )
        .expect("nested parses");
        let quoted =
            Theme::parse("quoted", "[tokens]\n\"diff.add\" = \"blue\"\n", &minimal()).expect("ok");
        assert_eq!(nested.declared(Token::DiffAdd), Color::Blue);
        assert_eq!(quoted.declared(Token::DiffAdd), Color::Blue);
    }

    #[test]
    fn a_bare_integer_is_a_palette_index() {
        let theme = Theme::parse("indexed", "[tokens]\nmuted = 246\n", &minimal()).expect("parses");
        assert_eq!(theme.declared(Token::Muted), Color::Indexed(246));
    }

    #[test]
    fn a_typo_in_a_theme_file_is_an_error_not_a_shrug() {
        let err = Theme::parse("typo", "[tokens]\nacent = \"red\"\n", &minimal())
            .expect_err("unknown token rejected");
        assert!(format!("{err:#}").contains("acent"), "{err:#}");

        let err = Theme::parse("bad-color", "[tokens]\naccent = \"puce\"\n", &minimal())
            .expect_err("unknown color rejected");
        assert!(format!("{err:#}").contains("accent"), "{err:#}");

        let err = Theme::parse("bad-border", "border = \"hexagon\"\n", &minimal())
            .expect_err("unknown border rejected");
        assert!(format!("{err:#}").contains("hexagon"), "{err:#}");
    }

    #[test]
    fn loading_an_unknown_theme_lists_what_there_is() {
        let err = load("nope").expect_err("unknown theme");
        let text = format!("{err:#}");
        assert!(text.contains("minimal") && text.contains("codex"), "{text}");
    }

    #[test]
    fn a_pinned_theme_wins_over_the_process_wide_one_and_unwinds() {
        // The outer pin is what makes this deterministic: reading the
        // process-wide theme here would race every other test thread that
        // constructs an `App` (which installs a theme), so "unwinds" could
        // fail for a reason that has nothing to do with unwinding.
        let outer = Arc::new(load("minimal").expect("loads"));
        let _base = pin(outer.clone());

        let ember = Arc::new(load("codex").expect("loads"));
        {
            let _pin = pin(ember.clone());
            assert_eq!(active().name, "codex");
            // A swap while pinned lands on the pin, leaving other threads alone.
            set_active_by_name("minimal").expect("swaps");
            assert_eq!(active().name, "minimal");
        }
        assert_eq!(active().name, outer.name, "the previous pin is restored");
    }

    #[test]
    fn a_swap_keeps_the_depth_the_terminal_reported() {
        let _pin = pin(Arc::new(minimal().with_depth(ColorDepth::Ansi16)));
        let swapped = set_active_by_name("codex").expect("swaps");
        assert_eq!(swapped.depth(), ColorDepth::Ansi16);
        for token in Token::ALL {
            assert!(is_ansi16(swapped.color(token)), "{}", token.key());
        }
        set_color_depth(ColorDepth::Mono);
        assert_eq!(active().color(Token::Accent), Color::Reset);
    }

    #[test]
    fn init_installs_the_skins_palette_and_falls_back_loudly() {
        // A name that will not load leaves the default installed *and* says
        // so, because a palette that went missing must never cost anyone their
        // TUI. Asserted on the value `init` would install rather than on the
        // process-wide slot, which every other test thread writes too.
        let depth = ColorDepth::Ansi256;
        let (theme, warning) = init_theme(crate::skin::Skin::Grok.companion_theme(), depth);
        assert_eq!(theme.name, "grok");
        assert_eq!(warning, None);
        assert_eq!(theme.depth(), depth, "degraded to what the terminal has");

        let (theme, warning) = init_theme("chartreuse", depth);
        let warning = warning.expect("a bad name warns");
        assert!(warning.contains("chartreuse"), "{warning}");
        assert!(warning.contains(DEFAULT_THEME), "{warning}");
        assert_eq!(theme.name, DEFAULT_THEME, "and the TUI still has a palette");

        // The wrapper itself still loads and installs without complaining.
        assert_eq!(init(DEFAULT_THEME), None);
    }

    /// Adversarial: `init` runs on every `App::new`, including the ones a
    /// second renderer (the GUI, a test rendering a known palette) makes while
    /// it has deliberately pinned a theme for itself. Installing through
    /// [`set_active`] would land on that pin and silently replace it
    /// mid-render, which is the one thing [`pin`] exists to prevent.
    #[test]
    fn init_leaves_a_pinned_thread_alone() {
        let pinned = Arc::new(load("codex").expect("loads").with_depth(ColorDepth::Ansi16));
        let _pin = pin(Arc::clone(&pinned));

        assert_eq!(init(DEFAULT_THEME), None);
        assert_eq!(active().name, "codex", "the pin still owns this thread");
        assert_eq!(
            active().depth(),
            ColorDepth::Ansi16,
            "including the depth it was pinned with"
        );
        for token in Token::ALL {
            assert_eq!(
                active().color(token),
                pinned.color(token),
                "{}",
                token.key()
            );
        }
    }

    #[test]
    fn an_unknown_top_level_key_is_an_error_not_a_shrug() {
        // A capitalized or misspelled top-level key used to parse cleanly and
        // do nothing: the user sees rounded borders, concludes the theme
        // system is broken, and has nothing to debug with.
        for (source, typo) in [
            ("Border = \"double\"\n", "Border"),
            ("boarder = \"double\"\n", "boarder"),
            ("discription = \"mine\"\n", "discription"),
            ("[token]\naccent = \"red\"\n", "token"),
        ] {
            let err = match Theme::parse("typo", source, &minimal()) {
                Ok(_) => panic!("'{typo}' must be rejected, not ignored"),
                Err(err) => format!("{err:#}"),
            };
            assert!(err.contains(typo), "the error must name the key: {err}");
        }
        // The keys a theme really may carry still parse, in any combination.
        let full = "name = \"mine\"\ndescription = \"d\"\nborder = \"thick\"\n\
                    [tokens]\naccent = \"red\"\n";
        let theme = Theme::parse("mine", full, &minimal()).expect("valid keys parse");
        assert_eq!(theme.border, BorderType::Thick);
        assert_eq!(theme.declared(Token::Accent), Color::Red);
        // And both built-ins go through the same door.
        for (name, source) in BUILTINS {
            Theme::parse(name, source, &bare())
                .unwrap_or_else(|err| panic!("{name} must stay parseable: {err:#}"));
        }
    }

    #[test]
    fn every_shipped_palette_belongs_to_a_skin() {
        // The catalog is not a menu any more: each entry exists because some
        // skin names it, so an orphan is dead weight nothing can reach.
        for (name, _) in BUILTINS {
            assert!(
                crate::skin::Skin::ALL
                    .iter()
                    .any(|skin| skin.companion_theme() == name),
                "{name} is not any skin's companion palette"
            );
        }
    }
}
