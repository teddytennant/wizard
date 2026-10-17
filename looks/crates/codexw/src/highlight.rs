// OWNER: history-cells
//! Syntax highlighting (spec B.3.6) and the syntax theme (`/theme`). Ported from Codex's
//! `render/highlight.rs`: syntect with the two-face grammar and theme bundles, one process-wide
//! active theme that the picker can swap for a live preview, the foreground of each token as the
//! only style that reaches the terminal (bold survives, background and italic do not), and the
//! guardrails on input size.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{OnceLock, RwLock};

use ratatui::style::{Color as RtColor, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{
    Color as SyntectColor, FontStyle, Highlighter, Style as SyntectStyle, Theme, ThemeSet,
};
use syntect::parsing::{Scope, SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;
use two_face::theme::EmbeddedThemeName;

use crate::style::palette;

pub const MAX_HIGHLIGHT_BYTES: usize = 512 * 1024;
pub const MAX_HIGHLIGHT_LINES: usize = 10_000;
pub const MAX_HIGHLIGHT_LINE_BYTES: usize = 4 * 1024;

// bat's `ansi`, `base16` and `base16-256` themes put ANSI palette semantics in the alpha byte.
const ANSI_ALPHA_INDEX: u8 = 0x00;
const ANSI_ALPHA_DEFAULT: u8 = 0x01;

fn syntax_set() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(two_face::syntax::extra_newlines)
}

// ---- themes -----------------------------------------------------------------------------------

/// Map a kebab-case theme name to the bundled theme.
fn parse_theme_name(name: &str) -> Option<EmbeddedThemeName> {
    use EmbeddedThemeName as T;
    Some(match name {
        "ansi" => T::Ansi,
        "base16" => T::Base16,
        "base16-eighties-dark" => T::Base16EightiesDark,
        "base16-mocha-dark" => T::Base16MochaDark,
        "base16-ocean-dark" => T::Base16OceanDark,
        "base16-ocean-light" => T::Base16OceanLight,
        "base16-256" => T::Base16_256,
        "catppuccin-frappe" => T::CatppuccinFrappe,
        "catppuccin-latte" => T::CatppuccinLatte,
        "catppuccin-macchiato" => T::CatppuccinMacchiato,
        "catppuccin-mocha" => T::CatppuccinMocha,
        "coldark-cold" => T::ColdarkCold,
        "coldark-dark" => T::ColdarkDark,
        "dark-neon" => T::DarkNeon,
        "dracula" => T::Dracula,
        "github" => T::Github,
        "gruvbox-dark" => T::GruvboxDark,
        "gruvbox-light" => T::GruvboxLight,
        "inspired-github" => T::InspiredGithub,
        "1337" => T::Leet,
        "monokai-extended" => T::MonokaiExtended,
        "monokai-extended-bright" => T::MonokaiExtendedBright,
        "monokai-extended-light" => T::MonokaiExtendedLight,
        "monokai-extended-origin" => T::MonokaiExtendedOrigin,
        "nord" => T::Nord,
        "one-half-dark" => T::OneHalfDark,
        "one-half-light" => T::OneHalfLight,
        "solarized-dark" => T::SolarizedDark,
        "solarized-light" => T::SolarizedLight,
        "sublime-snazzy" => T::SublimeSnazzy,
        "two-dark" => T::TwoDark,
        "zenburn" => T::Zenburn,
        _ => return None,
    })
}

/// All 32 bundled theme names, alphabetical.
const BUILTIN_THEME_NAMES: &[&str] = &[
    "1337",
    "ansi",
    "base16",
    "base16-256",
    "base16-eighties-dark",
    "base16-mocha-dark",
    "base16-ocean-dark",
    "base16-ocean-light",
    "catppuccin-frappe",
    "catppuccin-latte",
    "catppuccin-macchiato",
    "catppuccin-mocha",
    "coldark-cold",
    "coldark-dark",
    "dark-neon",
    "dracula",
    "github",
    "gruvbox-dark",
    "gruvbox-light",
    "inspired-github",
    "monokai-extended",
    "monokai-extended-bright",
    "monokai-extended-light",
    "monokai-extended-origin",
    "nord",
    "one-half-dark",
    "one-half-light",
    "solarized-dark",
    "solarized-light",
    "sublime-snazzy",
    "two-dark",
    "zenburn",
];

/// `catppuccin-latte` on a light terminal, `catppuccin-mocha` otherwise.
pub fn adaptive_default_theme_name() -> &'static str {
    if palette().light_bg() {
        "catppuccin-latte"
    } else {
        "catppuccin-mocha"
    }
}

/// A theme available in the picker: bundled, or a `.tmTheme` file under `<config>/themes/`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThemeEntry {
    pub name: String,
    pub is_custom: bool,
}

fn custom_theme_path(name: &str, home: &Path) -> PathBuf {
    home.join("themes").join(format!("{name}.tmTheme"))
}

fn load_custom_theme(name: &str, home: &Path) -> Option<Theme> {
    ThemeSet::get_theme(custom_theme_path(name, home)).ok()
}

/// The bundled themes plus any valid custom ones, sorted case-insensitively.
pub fn list_available_themes(config_dir: Option<&Path>) -> Vec<ThemeEntry> {
    let mut entries: Vec<ThemeEntry> = BUILTIN_THEME_NAMES
        .iter()
        .map(|n| ThemeEntry {
            name: (*n).into(),
            is_custom: false,
        })
        .collect();
    if let Some(home) = config_dir
        && let Ok(rd) = std::fs::read_dir(home.join("themes"))
    {
        for e in rd.flatten() {
            let path = e.path();
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if path.extension().and_then(|x| x.to_str()) == Some("tmTheme")
                && ThemeSet::get_theme(&path).is_ok()
                && !entries.iter().any(|x| x.name == stem)
            {
                entries.push(ThemeEntry {
                    name: stem.into(),
                    is_custom: true,
                });
            }
        }
    }
    entries.sort_by_cached_key(|e| (e.name.to_ascii_lowercase(), e.name.clone()));
    entries
}

/// A theme by name: bundled first, then `<config>/themes/<name>.tmTheme`.
pub fn resolve_theme_by_name(name: &str, config_dir: Option<&Path>) -> Option<Theme> {
    if let Some(t) = parse_theme_name(name) {
        return Some(two_face::theme::extra().get(t).clone());
    }
    config_dir.and_then(|h| load_custom_theme(name, h))
}

/// Why a configured name does not resolve, in words for the user.
pub fn validate_theme_name(name: &str, config_dir: Option<&Path>) -> Option<String> {
    if resolve_theme_by_name(name, config_dir).is_some() {
        return None;
    }
    let dir = config_dir
        .map(|h| custom_theme_path(name, h).display().to_string())
        .unwrap_or_else(|| format!("~/.config/codexw/themes/{name}.tmTheme"));
    Some(format!(
        "Theme \"{name}\" not found. Using the default theme. To use a custom theme, place a .tmTheme file at {dir}."
    ))
}

// ---- the active theme --------------------------------------------------------------------------

/// A theme picked explicitly (the saved choice, or a live preview). `None` means the adaptive
/// default, which follows the terminal palette, so a light terminal gets Latte without asking.
static OVERRIDE: OnceLock<RwLock<Option<Theme>>> = OnceLock::new();
static CONFIGURED: OnceLock<RwLock<Option<String>>> = OnceLock::new();
static REVISION: AtomicU64 = AtomicU64::new(0);

fn override_lock() -> &'static RwLock<Option<Theme>> {
    OVERRIDE.get_or_init(|| RwLock::new(None))
}

fn configured_lock() -> &'static RwLock<Option<String>> {
    CONFIGURED.get_or_init(|| RwLock::new(None))
}

fn adaptive_default() -> Theme {
    static MOCHA: OnceLock<Theme> = OnceLock::new();
    static LATTE: OnceLock<Theme> = OnceLock::new();
    let (cell, name) = if palette().light_bg() {
        (&LATTE, EmbeddedThemeName::CatppuccinLatte)
    } else {
        (&MOCHA, EmbeddedThemeName::CatppuccinMocha)
    };
    cell.get_or_init(|| two_face::theme::extra().get(name).clone())
        .clone()
}

/// `~/.config/codexw`, where `config.toml` and `themes/` live.
pub fn config_dir() -> Option<PathBuf> {
    std::env::var_os("CODEXW_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config/codexw")))
}

/// The theme the user chose and saved, `None` for the adaptive default.
pub fn configured_theme() -> Option<String> {
    configured_lock().read().ok().and_then(|g| g.clone())
}

/// Make `name` the active theme and the saved choice (`None` goes back to the adaptive
/// default). A name that does not resolve changes nothing; the return value says why.
pub fn set_configured_theme(name: Option<String>, config_dir: Option<&Path>) -> Option<String> {
    let theme = match &name {
        Some(n) => match resolve_theme_by_name(n, config_dir) {
            Some(t) => Some(t),
            None => return validate_theme_name(n, config_dir),
        },
        None => None,
    };
    if let Ok(mut g) = configured_lock().write() {
        *g = name;
    }
    store_theme(theme);
    None
}

fn store_theme(theme: Option<Theme>) {
    let mut g = match override_lock().write() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    *g = theme;
    REVISION.fetch_add(1, Ordering::Release);
}

/// Swap the active theme (live preview) and invalidate anything cached from the old one.
pub fn set_syntax_theme(theme: Theme) {
    store_theme(Some(theme));
}

/// Put back what `current_syntax_theme` returned before a preview: `None` is the default.
pub fn restore_syntax_theme(previous: Option<Theme>) {
    store_theme(previous);
}

pub fn syntax_theme_revision() -> u64 {
    REVISION.load(Ordering::Acquire)
}

pub fn current_syntax_theme() -> Theme {
    match override_lock().read() {
        Ok(g) => g.clone().unwrap_or_else(adaptive_default),
        Err(p) => p.into_inner().clone().unwrap_or_else(adaptive_default),
    }
}

/// The explicit theme in force, for putting back after a preview; `None` while the adaptive
/// default is.
pub fn explicit_syntax_theme() -> Option<Theme> {
    match override_lock().read() {
        Ok(g) => g.clone(),
        Err(p) => p.into_inner().clone(),
    }
}

// ---- colours -----------------------------------------------------------------------------------

fn ansi_palette_color(index: u8) -> RtColor {
    match index {
        0x00 => RtColor::Black,
        0x01 => RtColor::Red,
        0x02 => RtColor::Green,
        0x03 => RtColor::Yellow,
        0x04 => RtColor::Blue,
        0x05 => RtColor::Magenta,
        0x06 => RtColor::Cyan,
        0x07 => RtColor::Gray,
        n => RtColor::Indexed(n),
    }
}

/// A syntect colour as a terminal colour; `None` means the terminal's own default.
fn convert_syntect_color(c: SyntectColor) -> Option<RtColor> {
    match c.a {
        ANSI_ALPHA_INDEX => Some(ansi_palette_color(c.r)),
        ANSI_ALPHA_DEFAULT => None,
        _ => Some(RtColor::Rgb(c.r, c.g, c.b)),
    }
}

/// Foreground and bold only: the terminal keeps its background, and italic and underline render
/// poorly (Dracula underlines type names).
fn convert_style(s: SyntectStyle) -> Style {
    let mut out = Style::default();
    if let Some(fg) = convert_syntect_color(s.foreground) {
        out = out.fg(fg);
    }
    if s.font_style.contains(FontStyle::BOLD) {
        out.add_modifier |= Modifier::BOLD;
    }
    out
}

/// The foreground the active theme gives the first of `scopes` that has one.
pub fn foreground_style_for_scopes(scopes: &[&str]) -> Option<Style> {
    let theme = current_syntax_theme();
    let h = Highlighter::new(&theme);
    scopes.iter().find_map(|name| {
        let scope = Scope::new(name).ok()?;
        let fg = h.style_mod_for_stack(&[scope]).foreground?;
        convert_syntect_color(fg).map(|c| Style::default().fg(c))
    })
}

/// Backgrounds the active theme gives inserted and deleted lines (`markup.inserted`,
/// `diff.inserted` and the deleted pair), as RGB.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DiffScopeBackgrounds {
    pub inserted: Option<(u8, u8, u8)>,
    pub deleted: Option<(u8, u8, u8)>,
}

pub fn diff_scope_backgrounds() -> DiffScopeBackgrounds {
    let theme = current_syntax_theme();
    let h = Highlighter::new(&theme);
    let bg = |name: &str| {
        let scope = Scope::new(name).ok()?;
        let b = h.style_mod_for_stack(&[scope]).background?;
        Some((b.r, b.g, b.b))
    };
    DiffScopeBackgrounds {
        inserted: bg("markup.inserted").or_else(|| bg("diff.inserted")),
        deleted: bg("markup.deleted").or_else(|| bg("diff.deleted")),
    }
}

/// Style of the table header row: the theme's type colour, bold.
pub fn table_header_style() -> Style {
    foreground_style_for_scopes(&["entity.name.type", "support.type", "variable"])
        .unwrap_or_default()
        .add_modifier(Modifier::BOLD)
}

/// Default text colour of the theme, used by diff content with no grammar token of its own.
pub fn text_style() -> Style {
    let theme = current_syntax_theme();
    let fg = theme.settings.foreground.and_then(convert_syntect_color);
    fg.map_or_else(Style::default, |c| Style::default().fg(c))
}

// ---- grammars ----------------------------------------------------------------------------------

/// Resolve a fence info string (`rust,no_run`) or extension to a grammar.
pub fn find_syntax(token: &str) -> Option<&'static SyntaxReference> {
    let ss = syntax_set();
    let t = token
        .trim()
        .trim_start_matches('.')
        .split([',', ' ', '\t'])
        .next()
        .unwrap_or("");
    if t.is_empty() {
        return None;
    }
    let lower = t.to_ascii_lowercase();
    let alias = match lower.as_str() {
        "csharp" | "c-sharp" => "c#",
        "cu" | "cuh" | "cppm" | "cxxm" | "ixx" => "cpp",
        "golang" => "go",
        "python3" => "python",
        "shell" => "bash",
        _ => t,
    };
    ss.find_syntax_by_token(alias)
        .or_else(|| ss.find_syntax_by_name(alias))
        .or_else(|| {
            let l = alias.to_ascii_lowercase();
            ss.syntaxes()
                .iter()
                .find(|s| s.name.to_ascii_lowercase() == l)
        })
        .or_else(|| ss.find_syntax_by_extension(t))
        .filter(|s| s.name != "Plain Text")
}

/// Grammar for a file path by extension. `.txt` resolves to Plain Text, which Codex still
/// runs through the theme, so it gets the text colour.
pub fn find_syntax_for_path(path: &str) -> Option<&'static SyntaxReference> {
    let file = path.rsplit('/').next().unwrap_or(path);
    let ext = file.rsplit_once('.').map(|(_, e)| e)?;
    find_syntax(ext)
}

// ---- highlighting ------------------------------------------------------------------------------

/// Spans for each line of `code`, or the plain-text fallback when it is too big or no grammar
/// applies (`syntax: None` runs Plain Text through the theme, giving the text colour). Lines come
/// back without their newline; an empty line is an empty span list.
pub fn highlight_spans(code: &str, syntax: Option<&SyntaxReference>) -> Vec<Vec<Span<'static>>> {
    let code = code.strip_suffix('\n').unwrap_or(code);
    let raw_lines: Vec<&str> = code.split('\n').collect();
    let too_big = code.len() > MAX_HIGHLIGHT_BYTES
        || raw_lines.len() > MAX_HIGHLIGHT_LINES
        || raw_lines.iter().any(|l| l.len() > MAX_HIGHLIGHT_LINE_BYTES);
    let text = text_style();
    let plain = |raw: &str| -> Vec<Span<'static>> {
        if raw.is_empty() {
            Vec::new()
        } else {
            vec![Span::styled(raw.to_string(), text)]
        }
    };
    let ss = syntax_set();
    let Some(syntax) = syntax
        .or_else(|| Some(ss.find_syntax_plain_text()))
        .filter(|_| !too_big)
    else {
        return raw_lines.iter().map(|l| plain(l)).collect();
    };
    let theme = current_syntax_theme();
    let mut h = HighlightLines::new(syntax, &theme);
    let mut out = Vec::with_capacity(raw_lines.len());
    for raw in LinesWithEndings::from(&format!("{code}\n")) {
        let Ok(ranges) = h.highlight_line(raw, ss) else {
            out.push(plain(raw.trim_end_matches(['\n', '\r'])));
            continue;
        };
        let mut spans: Vec<Span<'static>> = Vec::new();
        for (style, seg) in ranges {
            let seg = seg.trim_end_matches(['\n', '\r']);
            if seg.is_empty() {
                continue;
            }
            let style = convert_style(style);
            match spans.last_mut() {
                Some(prev) if prev.style == style => prev.content.to_mut().push_str(seg),
                _ => spans.push(Span::styled(seg.to_string(), style)),
            }
        }
        out.push(spans);
    }
    out.truncate(raw_lines.len());
    out
}

/// Highlight `code` as language `lang` (a fence info token). Unknown languages are plain text
/// in the default style, not the theme text colour (spec B.3.2: no SGR at all).
pub fn highlight_code_to_lines(code: &str, lang: &str) -> Vec<Line<'static>> {
    let code = code.strip_suffix('\n').unwrap_or(code);
    match find_syntax(lang) {
        Some(sx) => highlight_spans(code, Some(sx))
            .into_iter()
            .map(Line::from)
            .collect(),
        None => code
            .split('\n')
            .map(|l| Line::from(l.to_string()))
            .collect(),
    }
}

/// A shell command as highlighted lines (`• Ran ...` and the pager's `$ ...`).
pub fn highlight_bash_to_lines(script: &str) -> Vec<Line<'static>> {
    highlight_code_to_lines(script, "bash")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::{ColorLevel, Palette, set_palette};
    use ratatui::style::Color;
    use syntect::parsing::{ParseState, ScopeStack};

    fn dark() {
        set_palette(Palette::new(
            Some((230, 230, 230)),
            Some((0, 0, 0)),
            ColorLevel::TrueColor,
        ));
    }

    /// `[#rrggbb]text` runs, the notation of tools/ansi-runs.py.
    fn runs(line: &Line<'_>) -> String {
        let mut s = String::new();
        for sp in &line.spans {
            let c = match sp.style.fg {
                Some(Color::Rgb(r, g, b)) => format!("{r:02x}{g:02x}{b:02x}"),
                _ => "-".into(),
            };
            s.push_str(&format!("[{c}]{}", sp.content));
        }
        s
    }

    fn dump(lang: &str, code: &str) -> Vec<String> {
        dark();
        highlight_code_to_lines(code, lang)
            .iter()
            .map(runs)
            .collect()
    }

    #[test]
    #[ignore]
    fn print_runs() {
        for (l, c) in [
            (
                "rust",
                "pub fn add(a: i32, b: i32) -> i32 {\n    // wrap instead of panicking in debug builds\n    a.wrapping_add(b)\n}\n\n#[test]\nfn wraps() {\n    assert_eq!(add(i32::MAX, 1), i32::MIN);\n}\n",
            ),
            (
                "python",
                "def add(a: int, b: int) -> int:\n    return (a + b + 2**31) % 2**32 - 2**31\n",
            ),
            (
                "json",
                "{\"name\": \"demo\", \"version\": \"0.1.0\", \"private\": true}\n",
            ),
            ("diff", "-    a + b\n+    a.wrapping_add(b)\n"),
            ("bash", "echo hello from the shell"),
            (
                "bash",
                "sh -c 'echo running 4 tests; echo \"test add ... ok\"; echo \"test sub ... FAILED\" >&2; exit 101'",
            ),
            (
                "bash",
                "git status --short && printf 'a very long line %.0s' $(seq 1 30); echo; git log --oneline | head -3",
            ),
        ] {
            for line in dump(l, c) {
                println!("{line}");
            }
            println!();
        }
    }

    #[test]
    #[ignore]
    fn print_scopes() {
        for (l, c) in [
            (
                "rust",
                "pub fn add(a: i32, b: i32) -> i32 {\n    a.wrapping_add(b)\n}\n#[test]\nfn w() { assert_eq!(i32::MAX, 1); }\n",
            ),
            ("python", "def add(a: int, b: int) -> int:\n    return 1\n"),
            ("json", "{\"name\": \"demo\", \"p\": true}\n"),
            (
                "bash",
                "sh -c 'echo hi' && git log --oneline | head -3; echo $(seq 1 30)",
            ),
        ] {
            let ss = syntax_set();
            let mut st = ParseState::new(find_syntax(l).unwrap());
            let mut stack = ScopeStack::new();
            for raw in c.lines() {
                let t = format!("{raw}\n");
                let ops = st.parse_line(&t, ss).unwrap();
                let mut last = 0;
                for (pos, op) in ops {
                    if pos > last {
                        let names: Vec<String> =
                            stack.as_slice().iter().map(|x| x.build_string()).collect();
                        println!("{:?} => {}", &t[last..pos], names.join(" "));
                    }
                    last = pos;
                    stack.apply(&op).unwrap();
                }
            }
            println!();
        }
    }

    #[test]
    fn matches_the_reference_runs() {
        // Rows of reference/codex/120x36/turns-02-markdown-done.full.ansi and turns-05-exec-done.ansi.
        let rust = dump(
            "rust",
            "pub fn add(a: i32, b: i32) -> i32 {\n    a.wrapping_add(b)\n}\n#[test]\n",
        );
        assert_eq!(
            rust[0],
            "[cba6f7]pub[cdd6f4] [cba6f7]fn[cdd6f4] [89b4fa]add[9399b2]([eba0ac]a[9399b2]:[eba0ac] [cba6f7]i32[eba0ac], b[9399b2]:[eba0ac] [cba6f7]i32[9399b2])[cdd6f4] [9399b2]->[cdd6f4] [cba6f7]i32[cdd6f4] [9399b2]{"
        );
        assert_eq!(
            rust[1],
            "[cdd6f4]    a[94e2d5].[89b4fa]wrapping_add[9399b2]([cdd6f4]b[9399b2])"
        );
        assert_eq!(rust[3], "[fab387]#[f9e2af][test]");
        let json = dump("json", "{\"name\": \"demo\", \"private\": true}");
        assert_eq!(
            json[0],
            "[9399b2]{\"[89b4fa]name[9399b2]\":[cdd6f4] [a6e3a1]\"demo\"[9399b2],[cdd6f4] [9399b2]\"[89b4fa]private[9399b2]\":[cdd6f4] [fab387]true[9399b2]}"
        );
        let sh = dump(
            "bash",
            "git status --short && printf 'a b' $(seq 1 30); echo",
        );
        assert_eq!(
            sh[0],
            "[89b4fa]git[cdd6f4] status[9399b2] --[eba0ac]short[cdd6f4] [94e2d5]&&[cdd6f4] [89b4fa]printf[cdd6f4] [a6e3a1]'a b'[cdd6f4] [f38ba8]$[9399b2]([89b4fa]seq[cdd6f4] 1 30[9399b2])[94e2d5];[cdd6f4] [89b4fa]echo"
        );
    }

    #[test]
    fn table_header_is_mocha_yellow_bold() {
        dark();
        let s = table_header_style();
        assert_eq!(s.fg, Some(Color::Rgb(249, 226, 175)));
        assert!(s.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn unknown_language_is_unstyled() {
        let l = highlight_code_to_lines("plain fence", "");
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].spans[0].style, Style::default());
    }
}
