// OWNER: syntax (three tmThemes and the ANSI remap of the `terminal` theme)
//! Code highlighting: syntect's grammars with Grok Build's own `grok-night.tmTheme` rules, so
//! scopes take the colours the real binary gives them (`fn` `#bb9af7`, `println!(` `#9abdf5`,
//! strings `#9ece6a`, comments italic `#51597d`). Unknown languages and oversized blocks fall
//! back to the plain markdown text colour.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{Mutex, OnceLock};

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use syntect::easy::HighlightLines;
use syntect::highlighting::{
    Color as SColor, FontStyle, ScopeSelectors, StyleModifier, Theme as STheme, ThemeItem,
    ThemeSettings,
};
use syntect::parsing::{SyntaxReference, SyntaxSet};

use super::{grok_day_theme, grok_night_theme, tokyo_night_theme};
use crate::theme::Syntax;

const MAX_BYTES: usize = 128 * 1024;
const MAX_LINE: usize = 2000;

fn syntaxes() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn hex(v: u32) -> SColor {
    SColor {
        r: (v >> 16) as u8,
        g: (v >> 8) as u8,
        b: v as u8,
        a: 255,
    }
}

fn build(name: &str, default_fg: u32, rules: &[(&str, Option<u32>, Option<&str>)]) -> STheme {
    let mut scopes = Vec::new();
    for (sel, fg, fs) in rules {
        let Ok(scope) = ScopeSelectors::from_str(sel) else {
            continue;
        };
        let font_style = fs.map(|s| {
            let mut f = FontStyle::empty();
            for w in s.split_whitespace() {
                match w {
                    "bold" => f |= FontStyle::BOLD,
                    "italic" => f |= FontStyle::ITALIC,
                    "underline" => f |= FontStyle::UNDERLINE,
                    _ => {}
                }
            }
            f
        });
        scopes.push(ThemeItem {
            scope,
            style: StyleModifier {
                foreground: fg.map(hex),
                background: None,
                font_style,
            },
        });
    }
    STheme {
        name: Some(name.into()),
        author: None,
        settings: ThemeSettings {
            foreground: Some(hex(default_fg)),
            ..Default::default()
        },
        scopes,
    }
}

/// The tmTheme a palette uses. `Ansi` (the `terminal` theme) starts from grok-night and maps
/// every colour onto the 16 terminal colours afterwards.
fn theme(kind: Syntax) -> &'static STheme {
    static NIGHT: OnceLock<STheme> = OnceLock::new();
    static DAY: OnceLock<STheme> = OnceLock::new();
    static TOKYO: OnceLock<STheme> = OnceLock::new();
    match kind {
        Syntax::Night | Syntax::Ansi => NIGHT.get_or_init(|| {
            build(
                "Grok Night",
                grok_night_theme::DEFAULT_FG,
                grok_night_theme::RULES,
            )
        }),
        Syntax::Day => DAY.get_or_init(|| {
            build(
                "Grok Day",
                grok_day_theme::DEFAULT_FG,
                grok_day_theme::RULES,
            )
        }),
        Syntax::Tokyo => TOKYO.get_or_init(|| {
            build(
                "Tokyo Night",
                tokyo_night_theme::DEFAULT_FG,
                tokyo_night_theme::RULES,
            )
        }),
    }
}

/// `polarity_safe_syntax_fg`: a colour with little chroma is the terminal default, the rest
/// become one of six named colours by hue, so code stays readable on either polarity.
pub fn polarity_safe(r: u8, g: u8, b: u8) -> Color {
    let (ri, gi, bi) = (r as i32, g as i32, b as i32);
    let max = ri.max(gi).max(bi);
    let min = ri.min(gi).min(bi);
    let chroma = max - min;
    if chroma < 40 {
        return Color::Reset;
    }
    let mut hue = if max == ri {
        (gi - bi) * 60 / chroma
    } else if max == gi {
        (bi - ri) * 60 / chroma + 120
    } else {
        (ri - gi) * 60 / chroma + 240
    };
    if hue < 0 {
        hue += 360;
    }
    // red 1, green 2, yellow 3, blue 4, magenta 5, cyan 6, as the fullscreen binary writes them
    Color::Indexed(match hue {
        0..30 | 330..=360 => 1,
        30..90 => 3,
        90..150 => 2,
        150..210 => 6,
        210..255 => 4,
        _ => 5,
    })
}

/// Load the grammars and the theme now, off the UI thread, so the first code block is not slow.
pub fn warm() {
    let _ = syntaxes();
    let _ = theme(Syntax::Night);
}

/// Fence info string or file extension to a grammar.
pub fn find(token: &str) -> Option<&'static SyntaxReference> {
    let ss = syntaxes();
    let t = token.trim().to_ascii_lowercase();
    let t = match t.as_str() {
        "" | "text" | "txt" | "plain" | "plaintext" => return None,
        "shell" | "sh" | "zsh" | "console" | "shellscript" => "bash",
        "rust" => "rs",
        "python" | "py3" => "py",
        "javascript" | "jsx" | "node" => "js",
        "typescript" | "tsx" => "ts",
        "yml" => "yaml",
        "golang" => "go",
        "c++" => "cpp",
        "patch" => "diff",
        other => other,
    };
    ss.find_syntax_by_token(t)
        .or_else(|| ss.find_syntax_by_extension(t))
}

fn plain(code: &str, fg: Color) -> Vec<Vec<Span<'static>>> {
    code.split('\n')
        .map(|l| {
            let l = l.strip_suffix('\r').unwrap_or(l);
            if l.is_empty() {
                Vec::new()
            } else {
                vec![Span::styled(l.to_string(), Style::new().fg(fg))]
            }
        })
        .collect()
}

type Highlighted = Vec<Vec<Span<'static>>>;
type HlCache = Mutex<HashMap<(Syntax, String, u64), Highlighted>>;

fn cache() -> &'static HlCache {
    static C: OnceLock<HlCache> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

fn hash(s: &str) -> u64 {
    // FNV-1a: cheap and stable, the cache only has to tell blocks apart
    let mut h = 0xcbf29ce484222325u64;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// One span list per line of `code` (no trailing newline). `fallback` colours text that has no
/// grammar.
pub fn highlight(kind: Syntax, lang: &str, code: &str, fallback: Color) -> Vec<Vec<Span<'static>>> {
    let code = code.strip_suffix('\n').unwrap_or(code);
    let Some(syntax) = find(lang) else {
        return plain(code, fallback);
    };
    if code.len() > MAX_BYTES {
        return plain(code, fallback);
    }
    let key = (kind, lang.to_ascii_lowercase(), hash(code));
    if let Some(hit) = cache().lock().ok().and_then(|c| c.get(&key).cloned()) {
        return hit;
    }
    let ss = syntaxes();
    let mut hl = HighlightLines::new(syntax, theme(kind));
    let mut out = Vec::new();
    for raw in code.split('\n') {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        if raw.is_empty() {
            out.push(Vec::new());
            continue;
        }
        if raw.len() > MAX_LINE {
            out.push(vec![Span::styled(
                raw.to_string(),
                Style::new().fg(fallback),
            )]);
            continue;
        }
        let mut line = String::with_capacity(raw.len() + 1);
        line.push_str(raw);
        line.push('\n');
        let Ok(parts) = hl.highlight_line(&line, ss) else {
            out.push(vec![Span::styled(
                raw.to_string(),
                Style::new().fg(fallback),
            )]);
            continue;
        };
        let mut spans: Vec<Span<'static>> = Vec::new();
        for (st, text) in parts {
            let text = text.trim_end_matches('\n');
            if text.is_empty() {
                continue;
            }
            let (r, g, b) = (st.foreground.r, st.foreground.g, st.foreground.b);
            let mut style = Style::new().fg(if kind == Syntax::Ansi {
                polarity_safe(r, g, b)
            } else {
                Color::Rgb(r, g, b)
            });
            if st.font_style.contains(FontStyle::ITALIC) {
                style = style.add_modifier(Modifier::ITALIC);
            }
            if st.font_style.contains(FontStyle::BOLD) {
                style = style.add_modifier(Modifier::BOLD);
            }
            match spans.last_mut() {
                Some(prev) if prev.style == style => prev.content.to_mut().push_str(text),
                _ => spans.push(Span::styled(text.to_string(), style)),
            }
        }
        out.push(spans);
    }
    if let Ok(mut c) = cache().lock() {
        if c.len() > 256 {
            c.clear();
        }
        c.insert(key, out.clone());
    }
    out
}

/// Language token for a file path, by extension.
pub fn lang_for_path(path: &str) -> &str {
    let file = path.rsplit('/').next().unwrap_or(path);
    file.rsplit_once('.').map_or("", |(_, e)| e)
}
