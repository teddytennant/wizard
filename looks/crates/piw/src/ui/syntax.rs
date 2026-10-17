//! Code highlighting as Pi does it: highlight.js 10.7.3 tokenises (`ui::hljs`), and Pi's
//! `buildCliHighlightTheme` decides the colour of each token: keywords and `name` take
//! `syntaxKeyword`, built-ins and types `syntaxType`, numbers and literals `syntaxNumber`,
//! strings and regexps `syntaxString`, comments `syntaxComment`, titles `syntaxFunction`,
//! attributes and parameters `syntaxVariable`, `meta` is muted. Text outside any token is drawn
//! in the default colour.

use ratatui::style::{Modifier, Style};
use ratatui::text::Span;

use super::hljs::{self, Fmt};
use crate::theme::PiTheme;

/// Blocks bigger than this are drawn plain: highlighting is the one super-linear cost of a frame.
const MAX_BYTES: usize = 128 * 1024;
const MAX_LINE: usize = 2000;

/// Language name for a file path by extension (Pi's `getLanguageFromPath`).
pub fn lang_for_path(path: &str) -> Option<&'static str> {
    // Pi takes whatever follows the last dot of the whole path, or the whole path when there is
    // no dot, so only a bare `Makefile` or `Dockerfile` matches by name
    let ext = path.rsplit('.').next().unwrap_or(path);
    Some(match ext.to_ascii_lowercase().as_str() {
        "ts" | "tsx" => "typescript",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "py" => "python",
        "rb" => "ruby",
        "rs" => "rust",
        "go" => "go",
        "java" => "java",
        "kt" => "kotlin",
        "swift" => "swift",
        "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" => "cpp",
        "cs" => "csharp",
        "php" => "php",
        "sh" | "bash" | "zsh" => "bash",
        "fish" => "fish",
        "ps1" => "powershell",
        "sql" => "sql",
        "html" | "htm" => "html",
        "css" => "css",
        "scss" => "scss",
        "sass" => "sass",
        "less" => "less",
        "json" => "json",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        "xml" => "xml",
        "md" | "markdown" => "markdown",
        "dockerfile" => "dockerfile",
        "makefile" => "makefile",
        "cmake" => "cmake",
        "lua" => "lua",
        "perl" => "perl",
        "r" => "r",
        "scala" => "scala",
        "clj" => "clojure",
        "ex" | "exs" => "elixir",
        "erl" => "erlang",
        "hs" => "haskell",
        "ml" => "ocaml",
        "vim" => "vim",
        "graphql" => "graphql",
        "proto" => "protobuf",
        "tf" | "hcl" => "hcl",
        _ => return None,
    })
}

/// The language a fence info string or tool language names: the text up to the first `,`, `{`
/// or whitespace, which highlight.js accepts as a name or an alias in any case.
fn find_lang(lang: &str) -> Option<String> {
    let t = lang
        .split(|c: char| c == ',' || c == '{' || c.is_whitespace())
        .next()
        .unwrap_or("");
    (!t.is_empty() && hljs::supports(t)).then(|| t.to_string())
}

fn style_of(fmt: Option<Fmt>, theme: &PiTheme) -> Style {
    match fmt {
        None => Style::default(),
        Some(Fmt::Tok(t)) => theme.fg(t),
        Some(Fmt::Italic) => Style::default().add_modifier(Modifier::ITALIC),
        Some(Fmt::Bold) => Style::default().add_modifier(Modifier::BOLD),
        Some(Fmt::Underline) => Style::default().add_modifier(Modifier::UNDERLINED),
    }
}

fn plain(line: &str) -> Vec<Span<'static>> {
    if line.is_empty() {
        vec![]
    } else {
        vec![Span::raw(line.to_string())]
    }
}

/// One span list per line of `code` (no newlines in the spans), or `None` for a language Pi has
/// no grammar for.
pub fn highlight(lang: &str, code: &str, theme: &PiTheme) -> Option<Vec<Vec<Span<'static>>>> {
    let name = find_lang(lang)?;
    if code.len() > MAX_BYTES {
        hljs::get_language(&name)?;
        return Some(code.split('\n').map(plain).collect());
    }
    // The engine is a port of someone else's state machine, run on whatever a model wrote. If it
    // ever trips, draw the block plain the way Pi does when highlight.js throws.
    let rows = match std::panic::catch_unwind(|| hljs::highlight_rows(&name, code)) {
        Ok(rows) => rows?,
        Err(_) => return Some(code.split('\n').map(plain).collect()),
    };
    Some(
        rows.into_iter()
            .map(|row| {
                if row.iter().map(|(t, _)| t.len()).sum::<usize>() > MAX_LINE {
                    let line: String = row.iter().map(|(t, _)| t.as_str()).collect();
                    return plain(&line);
                }
                row.into_iter()
                    .map(|(t, f)| Span::styled(t, style_of(f, theme)))
                    .collect()
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Tok;

    fn dark() -> PiTheme {
        PiTheme::dark()
    }

    fn colour_of(
        lines: &[Vec<Span<'static>>],
        row: usize,
        text: &str,
    ) -> Option<ratatui::style::Color> {
        lines[row]
            .iter()
            .find(|s| s.content.trim() == text)
            .and_then(|s| s.style.fg)
    }

    #[test]
    fn rust_follows_highlight_js() {
        let t = dark();
        let code =
            "fn main() {\n    // say hi\n    let n: u32 = 42;\n    println!(\"hi {}\", n);\n}";
        let l = highlight("rust", code, &t).unwrap();
        assert_eq!(colour_of(&l, 0, "fn"), t.fg(Tok::SyntaxKeyword).fg);
        assert_eq!(colour_of(&l, 1, "// say hi"), t.fg(Tok::SyntaxComment).fg);
        assert_eq!(colour_of(&l, 2, "let"), t.fg(Tok::SyntaxKeyword).fg);
        assert_eq!(colour_of(&l, 2, "u32"), t.fg(Tok::SyntaxType).fg);
        assert_eq!(colour_of(&l, 2, "42"), t.fg(Tok::SyntaxNumber).fg);
        assert_eq!(colour_of(&l, 3, "\"hi {}\""), t.fg(Tok::SyntaxString).fg);
    }

    #[test]
    fn a_token_that_spans_rows_keeps_its_colour_on_the_first_row_only() {
        let t = dark();
        let l = highlight("js", "/* a\n b */ x", &t).unwrap();
        assert_eq!(colour_of(&l, 0, "/* a"), t.fg(Tok::SyntaxComment).fg);
        assert_eq!(l[1][0].style, Style::default());
    }

    #[test]
    fn names_and_aliases_come_from_highlight_js() {
        let t = dark();
        assert!(highlight("nosuchlang", "x", &t).is_none());
        assert!(highlight("", "x", &t).is_none());
        // `text` is an alias of plaintext, which Pi draws in the default colour
        let plain = highlight("TEXT", "a b", &t).unwrap();
        assert_eq!(plain[0][0].style, Style::default());
        let ts = highlight("ts,linenos", "type A = number", &t).unwrap();
        assert_eq!(colour_of(&ts, 0, "type"), t.fg(Tok::SyntaxKeyword).fg);
    }

    #[test]
    fn paths_map_the_way_pi_does() {
        assert_eq!(lang_for_path("src/main.rs"), Some("rust"));
        assert_eq!(lang_for_path("a/b.tsx"), Some("typescript"));
        assert_eq!(lang_for_path("README"), None);
        assert_eq!(lang_for_path("Makefile"), Some("makefile"));
        // the whole path is split on dots, so a directory with a dot hides the file name
        assert_eq!(lang_for_path("v1.2/Makefile"), None);
    }

    #[test]
    fn long_lines_and_big_blocks_are_drawn_plain() {
        let t = dark();
        let long = format!("let x = \"{}\";", "a".repeat(MAX_LINE));
        let l = highlight("js", &format!("{long}\nreturn 1"), &t).unwrap();
        assert_eq!(l[0].len(), 1);
        assert_eq!(l[0][0].style, Style::default());
        assert_eq!(colour_of(&l, 1, "return"), t.fg(Tok::SyntaxKeyword).fg);
        let big = "return 1\n".repeat(MAX_BYTES / 9 + 1);
        let l = highlight("js", &big, &t).unwrap();
        assert!(l
            .iter()
            .all(|r| r.iter().all(|s| s.style == Style::default())));
    }
}
