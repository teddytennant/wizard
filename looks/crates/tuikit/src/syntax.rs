//! Syntax highlighting with the *theme's* syntax tokens for colour.
//!
//! Two engines. With [`set_opencode_parity`] on (openw), languages opencode highlights and we
//! ship a grammar for go through tree-sitter ([`crate::ts`]) with opencode's own queries and
//! scope table, so the colours match cell for cell, and a language opencode has no parser for
//! stays plain. Otherwise, and for languages without a shipped grammar, syntect's grammars are
//! used: each scope on the stack is classified once into a small set of roles (keyword, string,
//! function, ...) and the role picks a token from [`Theme`], with the italic/bold choices
//! opencode makes in `getSyntaxRules`. openc stays on syntect because its streaming code fences
//! highlight one line at a time ([`LineHighlighter`]), which tree-sitter cannot do.

use crate::theme::Theme;
use crate::ts;
use ratatui::style::{Modifier, Style};
use ratatui::text::Span;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxReference, SyntaxSet};

/// Blocks bigger than this are drawn plain; highlighting is the only super-linear-ish cost
/// in rendering a long transcript.
pub const MAX_HIGHLIGHT_BYTES: usize = 256 * 1024;
const MAX_LINE_BYTES: usize = 2000;

static PARITY: AtomicBool = AtomicBool::new(false);

/// Draw code as opencode does: tree-sitter with its queries, and a language it has no parser
/// for stays in the plain text colour instead of being coloured by the fallback grammar. Off by
/// default, so other frontends keep highlighting everything syntect knows.
pub fn set_opencode_parity(on: bool) {
    PARITY.store(on, Ordering::Relaxed);
}

fn parity() -> bool {
    PARITY.load(Ordering::Relaxed)
}

/// A grammar to highlight with.
#[derive(Clone, Copy)]
pub struct Syntax(Inner);

#[derive(Clone, Copy)]
enum Inner {
    Ts(&'static ts::Lang),
    Sublime(&'static SyntaxReference),
}

impl Syntax {
    /// Name of the tree-sitter grammar in use, `None` for the syntect fallback.
    pub fn tree_sitter(&self) -> Option<&'static str> {
        match self.0 {
            Inner::Ts(l) => Some(l.name),
            Inner::Sublime(_) => None,
        }
    }

    /// The syntect grammar for the same language, for the one-line-at-a-time path.
    fn sublime(&self) -> Option<&'static SyntaxReference> {
        match self.0 {
            Inner::Sublime(s) => Some(s),
            Inner::Ts(l) => sublime_syntax(l.name),
        }
    }
}

fn syntax_set() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Role {
    Comment,
    String,
    Escape,
    Number,
    Keyword,
    KeywordUpright,
    Function,
    Type,
    Variable,
    Builtin,
    Operator,
    Punctuation,
    Heading,
    Strong,
    Emph,
    Raw,
    Link,
    Inserted,
    Deleted,
}

fn classify(name: &str) -> Option<Role> {
    let starts = |p: &str| name.starts_with(p);
    let has = |p: &str| name.contains(p);
    // Delimiters like the quotes of a string take the colour of what they delimit.
    if starts("punctuation.definition") || starts("meta.") || starts("source.") || starts("text.") {
        return None;
    }
    Some(match () {
        _ if starts("comment") => Role::Comment,
        // `{}` in a format string is still part of the string in opencode's Rust captures
        _ if starts("constant.other.placeholder") => Role::String,
        _ if starts("constant.character.escape") || starts("string.regexp") => Role::Escape,
        _ if starts("string") || starts("constant.character") => Role::String,
        _ if starts("constant.numeric")
            || starts("constant.language")
            || starts("support.constant") =>
        {
            Role::Number
        }
        _ if starts("constant") => Role::Number,
        _ if starts("keyword.operator") => Role::Operator,
        _ if starts("keyword.control.import") || starts("keyword.control.directive") => {
            Role::KeywordUpright
        }
        _ if starts("keyword") => Role::Keyword,
        _ if starts("storage.type.numeric")
            || starts("storage.type.primitive")
            || starts("storage.type.built") =>
        {
            Role::Type
        }
        // `fn` takes the function colour and macro names the keyword one, as in opencode's
        // tree-sitter Rust captures.
        _ if starts("storage.type.function") && has(".rust") => Role::Function,
        _ if starts("support.macro") || has("entity.name.function.macro") => Role::Keyword,
        _ if starts("storage") => Role::Keyword,
        _ if starts("entity.name.function")
            || starts("support.function")
            || starts("variable.function") =>
        {
            Role::Function
        }
        _ if starts("entity.name.tag") => Role::Keyword,
        _ if starts("entity.other.attribute-name") => Role::Function,
        _ if starts("entity.name")
            || starts("entity.other.inherited-class")
            || starts("support.type")
            || starts("support.class") =>
        {
            Role::Type
        }
        _ if starts("variable.language") => Role::Builtin,
        _ if starts("variable") => Role::Variable,
        _ if starts("punctuation.separator")
            || starts("punctuation.terminator")
            || starts("punctuation.accessor") =>
        {
            Role::Operator
        }
        _ if starts("punctuation") => Role::Punctuation,
        _ if starts("markup.heading") => Role::Heading,
        _ if starts("markup.bold") => Role::Strong,
        _ if starts("markup.italic") => Role::Emph,
        _ if starts("markup.raw") => Role::Raw,
        _ if starts("markup.underline.link") => Role::Link,
        _ if starts("markup.inserted") => Role::Inserted,
        _ if starts("markup.deleted") => Role::Deleted,
        _ if has("invalid") => Role::Builtin,
        _ => return None,
    })
}

fn role_style(role: Option<Role>, t: &Theme) -> Style {
    let s = Style::new();
    match role {
        None => s.fg(t.text),
        Some(Role::Comment) => s.fg(t.syntax_comment).add_modifier(Modifier::ITALIC),
        Some(Role::String) => s.fg(t.syntax_string),
        Some(Role::Escape) => s.fg(t.syntax_keyword),
        Some(Role::Number) => s.fg(t.syntax_number),
        Some(Role::Keyword) => s.fg(t.syntax_keyword).add_modifier(Modifier::ITALIC),
        Some(Role::KeywordUpright) => s.fg(t.syntax_keyword),
        Some(Role::Function) => s.fg(t.syntax_function),
        Some(Role::Type) => s.fg(t.syntax_type),
        Some(Role::Variable) => s.fg(t.syntax_variable),
        Some(Role::Builtin) => s.fg(t.error),
        Some(Role::Operator) => s.fg(t.syntax_operator),
        Some(Role::Punctuation) => s.fg(t.syntax_punctuation),
        Some(Role::Heading) => s.fg(t.markdown_heading).add_modifier(Modifier::BOLD),
        Some(Role::Strong) => s.fg(t.markdown_strong).add_modifier(Modifier::BOLD),
        Some(Role::Emph) => s.fg(t.markdown_emph).add_modifier(Modifier::ITALIC),
        Some(Role::Raw) => s.fg(t.markdown_code),
        Some(Role::Link) => s.fg(t.markdown_link).add_modifier(Modifier::UNDERLINED),
        Some(Role::Inserted) => s.fg(t.diff_added),
        Some(Role::Deleted) => s.fg(t.diff_removed),
    }
}

/// Resolve a fence info string or file extension to a grammar.
pub fn find_syntax(token: &str) -> Option<Syntax> {
    if parity() {
        let ft = ts::filetype_for_info(token);
        if let Some(l) = ft.as_deref().and_then(ts::lang) {
            return Some(Syntax(Inner::Ts(l)));
        }
        if !ft.as_deref().is_some_and(ts::opencode_has_parser) {
            return None;
        }
    }
    sublime_syntax(token).map(|s| Syntax(Inner::Sublime(s)))
}

fn sublime_syntax(token: &str) -> Option<&'static SyntaxReference> {
    let ss = syntax_set();
    let raw = token.trim().trim_start_matches('.');
    // Info strings can carry attributes: "rust,no_run", "python title=x".
    let t = raw
        .split(|c: char| c == ',' || c == '{' || c.is_whitespace())
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if t.is_empty() {
        return None;
    }
    let alias = match t.as_str() {
        "ts" | "tsx" | "typescript" | "jsx" | "mjs" | "cjs" | "node" | "javascript" => "js",
        "shell" | "sh" | "bash" | "zsh" | "fish" | "console" | "shellscript" => "sh",
        "py" | "python3" => "py",
        "rs" => "rs",
        "yml" => "yaml",
        "c++" | "cc" | "hpp" => "cpp",
        "golang" => "go",
        "rb" => "rb",
        "patch" => "diff",
        "html" | "htm" | "vue" | "svelte" => "html",
        "jsonc" | "json5" | "jsonl" => "json",
        "md" => "md",
        "latex" => "tex",
        other => other,
    };
    ss.find_syntax_by_token(alias)
        .or_else(|| ss.find_syntax_by_extension(alias))
        .filter(|s| s.name != "Plain Text")
}

/// Grammar for a path, by extension or well-known file name.
pub fn find_syntax_for_path(path: &str) -> Option<Syntax> {
    if parity() {
        let ft = ts::filetype_for_path(path);
        if let Some(l) = ft.and_then(ts::lang) {
            return Some(Syntax(Inner::Ts(l)));
        }
        if !ft.is_some_and(ts::opencode_has_parser) {
            return None;
        }
    }
    sublime_for_path(path).map(|s| Syntax(Inner::Sublime(s)))
}

fn sublime_for_path(path: &str) -> Option<&'static SyntaxReference> {
    let file = path.rsplit('/').next().unwrap_or(path);
    let ss = syntax_set();
    match file {
        "Makefile" | "makefile" | "GNUmakefile" => return ss.find_syntax_by_extension("make"),
        "Dockerfile" => return None,
        _ => {}
    }
    let ext = file.rsplit_once('.').map(|(_, e)| e)?;
    sublime_syntax(ext)
}

/// Highlights one line at a time and remembers the grammar state between lines, so a block that
/// is still growing can be highlighted by feeding it only its new lines. [`highlight`] is this
/// over every line of a finished text.
pub struct LineHighlighter {
    state: Option<ParseState>,
    stack: ScopeStack,
    roles: HashMap<Scope, Option<Role>>,
}

impl LineHighlighter {
    pub fn new(syntax: Option<Syntax>) -> Self {
        LineHighlighter {
            state: syntax.and_then(|s| s.sublime()).map(ParseState::new),
            stack: ScopeStack::new(),
            roles: HashMap::new(),
        }
    }

    /// Spans for `raw` (no newline), advancing the state. Lines that are empty, too long, or
    /// that the grammar cannot parse come back plain.
    pub fn line(&mut self, raw: &str, theme: &Theme) -> Vec<Span<'static>> {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        let plain_style = role_style(None, theme);
        if raw.is_empty() {
            return Vec::new();
        }
        let Some(state) = self.state.as_mut() else {
            return vec![Span::styled(raw.to_string(), plain_style)];
        };
        if raw.len() > MAX_LINE_BYTES {
            return vec![Span::styled(raw.to_string(), plain_style)];
        }
        let mut with_nl = String::with_capacity(raw.len() + 1);
        with_nl.push_str(raw);
        with_nl.push('\n');
        let Ok(ops) = state.parse_line(&with_nl, syntax_set()) else {
            return vec![Span::styled(raw.to_string(), plain_style)];
        };
        let stack = &mut self.stack;
        let roles = &mut self.roles;
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut last = 0usize;
        let emit = |text: &str,
                    stack: &ScopeStack,
                    roles: &mut HashMap<Scope, Option<Role>>,
                    spans: &mut Vec<Span<'static>>| {
            let text = text.trim_end_matches('\n');
            if text.is_empty() {
                return;
            }
            let role = stack.as_slice().iter().rev().find_map(|sc| {
                *roles
                    .entry(*sc)
                    .or_insert_with(|| classify(&sc.build_string()))
            });
            let style = role_style(role, theme);
            match spans.last_mut() {
                Some(prev) if prev.style == style => prev.content.to_mut().push_str(text),
                _ => spans.push(Span::styled(text.to_string(), style)),
            }
        };
        for (pos, op) in ops {
            let pos = pos.min(with_nl.len());
            if pos > last {
                emit(&with_nl[last..pos], stack, roles, &mut spans);
            }
            last = pos;
            if stack.apply(&op).is_err() {
                break;
            }
        }
        if last < with_nl.len() {
            emit(&with_nl[last..], stack, roles, &mut spans);
        }
        spans
    }

    /// Like [`line`](Self::line) but leaves the state as it was: for the last line of a growing
    /// block, which has not ended yet.
    pub fn preview(&mut self, raw: &str, theme: &Theme) -> Vec<Span<'static>> {
        let (state, stack) = (self.state.clone(), self.stack.clone());
        let out = self.line(raw, theme);
        self.state = state;
        self.stack = stack;
        out
    }
}

/// Highlight `code` into one span list per line (no trailing newlines). Unknown or missing
/// languages, oversized input, and parse errors all fall back to plain `text` colour, never
/// an error.
pub fn highlight(syntax: Option<Syntax>, code: &str, theme: &Theme) -> Vec<Vec<Span<'static>>> {
    let code = code.strip_suffix('\n').unwrap_or(code);
    let syntax = syntax.filter(|_| code.len() <= MAX_HIGHLIGHT_BYTES);
    let mut hl = LineHighlighter::new(syntax);
    if let Some(Syntax(Inner::Ts(l))) = syntax {
        // a block too big to parse is drawn plain, never half in another engine's colours
        hl = LineHighlighter::new(None);
        if let Some(lines) = ts::highlight(l, code, theme) {
            return lines;
        }
    }
    code.split('\n').map(|raw| hl.line(raw, theme)).collect()
}

/// Convenience: language token (fence info string) in, highlighted lines out.
pub fn highlight_token(lang: &str, code: &str, theme: &Theme) -> Vec<Vec<Span<'static>>> {
    highlight(find_syntax(lang), code, theme)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    fn theme() -> Theme {
        Theme::builtin("opencode").unwrap()
    }

    fn color_of(lines: &[Vec<Span<'static>>], needle: &str) -> Option<Color> {
        lines
            .iter()
            .flatten()
            .find(|s| s.content.contains(needle))
            .and_then(|s| s.style.fg)
    }

    #[test]
    fn rust_tokens_get_theme_colours() {
        let t = theme();
        let l = highlight_token(
            "rust",
            "// note\nfn main() {\n    let s = \"hi\"; let n = 42;\n}\n",
            &t,
        );
        assert_eq!(l.len(), 4);
        assert_eq!(color_of(&l, "// note"), Some(t.syntax_comment));
        assert_eq!(color_of(&l, "fn"), Some(t.syntax_function));
        assert_eq!(color_of(&l, "main"), Some(t.syntax_function));
        assert_eq!(color_of(&l, "\"hi\""), Some(t.syntax_string));
        assert_eq!(color_of(&l, "42"), Some(t.syntax_number));
    }

    #[test]
    fn python_and_json_and_shell_highlight() {
        let t = theme();
        let py = highlight_token("py", "def f(x):\n    return 'a' + str(x)\n", &t);
        assert_eq!(color_of(&py, "def"), Some(t.syntax_keyword));
        assert_eq!(color_of(&py, "'a'"), Some(t.syntax_string));
        let js = highlight_token("json", "{\"k\": [1, true]}", &t);
        assert!(color_of(&js, "1").is_some());
        let sh = highlight_token("bash", "echo \"x\" # hi\n", &t);
        assert_eq!(color_of(&sh, "# hi"), Some(t.syntax_comment));
    }

    #[test]
    fn highlighted_text_round_trips() {
        let code = "fn a() {\n    1 + 2\n}\n\nlet é = \"日本\";";
        let l = highlight_token("rust", code, &theme());
        let joined: Vec<String> = l
            .iter()
            .map(|r| r.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(joined.join("\n"), code);
    }

    #[test]
    fn unknown_language_and_empty_fall_back_to_plain_text_colour() {
        let t = theme();
        let l = highlight_token("klingon", "a\n\nb", &t);
        assert_eq!(l.len(), 3);
        assert!(l[1].is_empty());
        assert_eq!(l[0][0].style.fg, Some(t.text));
        assert_eq!(highlight(None, "", &t).len(), 1);
    }

    #[test]
    fn aliases_and_info_strings_resolve() {
        for tok in [
            "rs",
            "rust,no_run",
            "ts",
            "sh",
            "yml",
            "py",
            "Python",
            "c++",
            "html",
            ".rs",
        ] {
            assert!(find_syntax(tok).is_some(), "{tok}");
        }
        assert!(find_syntax("").is_none());
        assert!(find_syntax_for_path("src/main.rs").is_some());
        assert!(find_syntax_for_path("a/b/Makefile").is_some());
        assert!(find_syntax_for_path("README").is_none());
    }

    #[test]
    fn crlf_giant_lines_and_odd_unicode_do_not_panic() {
        let t = theme();
        let l = highlight_token("rust", "let a = 1;\r\nlet b = 2;\r\n", &t);
        assert_eq!(l.len(), 2);
        assert!(!l[0].iter().any(|s| s.content.contains('\r')));
        let big_line = format!("let s = \"{}\";", "x".repeat(5000));
        assert_eq!(highlight_token("rust", &big_line, &t).len(), 1);
        let _ = highlight_token("rust", "let 👨‍👩‍👧 = \"e\u{301}\";", &t);
    }

    #[test]
    fn oversized_blocks_are_drawn_plain() {
        let t = theme();
        let code = "fn x() {}\n".repeat(30_000);
        let l = highlight_token("rust", &code, &t);
        assert_eq!(l[0][0].style.fg, Some(t.text));
    }

    #[test]
    fn feeding_lines_one_at_a_time_matches_highlighting_the_whole_text() {
        let t = theme();
        let code = "fn a() {\n    let s = \"x\"; // c\n\n    /* open\n       still */\n}";
        let whole = highlight_token("rust", code, &t);
        let mut hl = LineHighlighter::new(find_syntax("rust"));
        let by_line: Vec<_> = code.split('\n').map(|l| hl.line(l, &t)).collect();
        assert_eq!(by_line, whole);
        // a preview does not advance the state
        let mut hl = LineHighlighter::new(find_syntax("rust"));
        let _ = hl.preview("/* open", &t);
        assert_eq!(hl.line("let x = 1;", &t), whole_line("let x = 1;", &t));
    }

    fn whole_line(l: &str, t: &Theme) -> Vec<Span<'static>> {
        highlight_token("rust", l, t).remove(0)
    }
}
