//! Tree-sitter highlighting that reproduces what opencode's TUI draws.
//!
//! opencode (through OpenTUI) parses a code block with a tree-sitter grammar, runs an nvim-style
//! `highlights.scm` over it, and styles each capture through a scope table (`ts_rules.rs`). The
//! queries in `crates/tuikit/queries/` are the files opencode downloads or bundles, byte for
//! byte, and [`apply`] is a port of OpenTUI's `treeSitterToTextChunks`, so the colours come out
//! cell for cell the same, including its quirks: overlapping captures are layered shallowest
//! scope first, and a scope without its own style falls back to its first segment (`function.call`
//! to `function`).
//!
//! Unknown predicates (`#lua-match?`, `#has-ancestor?`) are ignored by both engines, which is why
//! the queries work unedited here too.

use crate::theme::Theme;
use crate::ts_rules::{Rule, Tok, RULES};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex, OnceLock};
use tree_sitter::{Language, Parser, Query, QueryCursor, StreamingIterator};

/// Blocks bigger than this are drawn plain, as before; parsing is the one super-linear-ish cost
/// of rendering a long transcript.
pub const MAX_BYTES: usize = 128 * 1024;

type LangFn = fn() -> Language;

struct Def {
    /// OpenTUI filetype name.
    name: &'static str,
    language: LangFn,
    query: &'static str,
}

macro_rules! lang {
    ($krate:ident) => {
        || Language::new($krate::LANGUAGE)
    };
    ($krate:ident, $konst:ident) => {
        || Language::new($krate::$konst)
    };
}

static DEFS: &[Def] = &[
    Def {
        name: "python",
        language: lang!(tree_sitter_python),
        query: include_str!("../queries/python.scm"),
    },
    Def {
        name: "rust",
        language: lang!(tree_sitter_rust),
        query: include_str!("../queries/rust.scm"),
    },
    Def {
        name: "go",
        language: lang!(tree_sitter_go),
        query: include_str!("../queries/go.scm"),
    },
    Def {
        name: "cpp",
        language: lang!(tree_sitter_cpp),
        query: include_str!("../queries/cpp.scm"),
    },
    Def {
        name: "csharp",
        language: lang!(tree_sitter_c_sharp),
        query: include_str!("../queries/csharp.scm"),
    },
    Def {
        name: "bash",
        language: lang!(tree_sitter_bash),
        query: include_str!("../queries/bash.scm"),
    },
    Def {
        name: "c",
        language: lang!(tree_sitter_c),
        query: include_str!("../queries/c.scm"),
    },
    Def {
        name: "java",
        language: lang!(tree_sitter_java),
        query: include_str!("../queries/java.scm"),
    },
    Def {
        name: "ruby",
        language: lang!(tree_sitter_ruby),
        query: include_str!("../queries/ruby.scm"),
    },
    Def {
        name: "php",
        language: lang!(tree_sitter_php, LANGUAGE_PHP),
        query: include_str!("../queries/php.scm"),
    },
    Def {
        name: "html",
        language: lang!(tree_sitter_html),
        query: include_str!("../queries/html.scm"),
    },
    Def {
        name: "json",
        language: lang!(tree_sitter_json),
        query: include_str!("../queries/json.scm"),
    },
    Def {
        name: "yaml",
        language: lang!(tree_sitter_yaml),
        query: include_str!("../queries/yaml.scm"),
    },
    Def {
        name: "css",
        language: lang!(tree_sitter_css),
        query: include_str!("../queries/css.scm"),
    },
    Def {
        name: "lua",
        language: lang!(tree_sitter_lua),
        query: include_str!("../queries/lua.scm"),
    },
    Def {
        name: "toml",
        language: lang!(tree_sitter_toml_ng),
        query: include_str!("../queries/toml.scm"),
    },
    Def {
        name: "nix",
        language: lang!(tree_sitter_nix),
        query: include_str!("../queries/nix.scm"),
    },
    Def {
        name: "make",
        language: lang!(tree_sitter_make),
        query: include_str!("../queries/make.scm"),
    },
    Def {
        name: "diff",
        language: lang!(tree_sitter_diff),
        query: include_str!("../queries/diff.scm"),
    },
    Def {
        name: "javascript",
        language: lang!(tree_sitter_javascript),
        query: include_str!("../queries/javascript.scm"),
    },
    Def {
        name: "typescript",
        language: lang!(tree_sitter_typescript, LANGUAGE_TYPESCRIPT),
        query: include_str!("../queries/typescript.scm"),
    },
];

/// Filetypes OpenTUI aliases onto another parser.
fn alias(name: &str) -> &str {
    match name {
        "typescriptreact" => "typescript",
        "javascriptreact" => "javascript",
        other => other,
    }
}

/// Every filetype opencode highlights (its `parsers-config.ts` plus OpenTUI's bundled ones).
/// Filetypes outside this list are drawn in the plain text colour there. `swift` is configured
/// but never coloured in 1.18.34 (a capture shows it plain), so it is left out on purpose.
const OPENCODE_PARSERS: &[&str] = &[
    "python",
    "rust",
    "go",
    "cpp",
    "csharp",
    "bash",
    "c",
    "java",
    "kotlin",
    "ruby",
    "php",
    "scala",
    "html",
    "vue",
    "hcl",
    "json",
    "yaml",
    "haskell",
    "css",
    "julia",
    "lua",
    "ocaml",
    "clojure",
    "toml",
    "nix",
    "diff",
    "elixir",
    "fsharp",
    "r",
    "make",
    "vim",
    "xml",
    "agda",
    "javascript",
    "javascriptreact",
    "typescript",
    "typescriptreact",
    "markdown",
    "markdown_inline",
    "zig",
];

/// A compiled grammar and query.
pub struct Lang {
    pub name: &'static str,
    language: Language,
    query: Query,
}

static CELLS: [OnceLock<Option<Lang>>; DEFS.len()] = [const { OnceLock::new() }; DEFS.len()];

/// The grammar for an OpenTUI filetype, compiled on first use. `None` when opencode has a
/// parser we do not ship or the query fails to compile against the grammar.
pub fn lang(filetype: &str) -> Option<&'static Lang> {
    let name = alias(filetype);
    let i = DEFS.iter().position(|d| d.name == name)?;
    CELLS[i]
        .get_or_init(|| {
            let d = &DEFS[i];
            let language = (d.language)();
            let query = Query::new(&language, d.query).ok()?;
            Some(Lang {
                name: d.name,
                language,
                query,
            })
        })
        .as_ref()
}

/// Does opencode highlight this filetype at all?
pub fn opencode_has_parser(filetype: &str) -> bool {
    OPENCODE_PARSERS.contains(&filetype)
}

/// Names of the shipped grammars, for tests and diagnostics.
pub fn shipped() -> impl Iterator<Item = &'static str> {
    DEFS.iter().map(|d| d.name)
}

// ---- filetype detection (OpenTUI `infoStringToFiletype`, opencode's `filetype`) -------------

/// OpenTUI's extension table.
const EXT: &[(&str, &str)] = &[
    ("astro", "astro"),
    ("bash", "bash"),
    ("c", "c"),
    ("cc", "cpp"),
    ("cjs", "javascript"),
    ("clj", "clojure"),
    ("cljs", "clojure"),
    ("cljc", "clojure"),
    ("cpp", "cpp"),
    ("cxx", "cpp"),
    ("cs", "csharp"),
    ("cts", "typescript"),
    ("ctsx", "typescriptreact"),
    ("dart", "dart"),
    ("diff", "diff"),
    ("edn", "clojure"),
    ("go", "go"),
    ("gemspec", "ruby"),
    ("groovy", "groovy"),
    ("h", "c"),
    ("handlebars", "handlebars"),
    ("hbs", "handlebars"),
    ("hpp", "cpp"),
    ("hxx", "cpp"),
    ("h++", "cpp"),
    ("hh", "cpp"),
    ("hrl", "erlang"),
    ("hs", "haskell"),
    ("htm", "html"),
    ("html", "html"),
    ("ini", "ini"),
    ("js", "javascript"),
    ("jsx", "javascriptreact"),
    ("jl", "julia"),
    ("json", "json"),
    ("ksh", "bash"),
    ("kt", "kotlin"),
    ("kts", "kotlin"),
    ("latex", "latex"),
    ("less", "less"),
    ("lua", "lua"),
    ("markdown", "markdown"),
    ("md", "markdown"),
    ("mdown", "markdown"),
    ("mkd", "markdown"),
    ("mjs", "javascript"),
    ("ml", "ocaml"),
    ("mli", "ocaml"),
    ("mts", "typescript"),
    ("mtsx", "typescriptreact"),
    ("patch", "diff"),
    ("php", "php"),
    ("pl", "perl"),
    ("pm", "perl"),
    ("ps1", "powershell"),
    ("psm1", "powershell"),
    ("py", "python"),
    ("pyi", "python"),
    ("r", "r"),
    ("rb", "ruby"),
    ("rake", "ruby"),
    ("rs", "rust"),
    ("ru", "ruby"),
    ("sass", "sass"),
    ("sc", "scala"),
    ("scala", "scala"),
    ("scss", "scss"),
    ("sh", "bash"),
    ("sql", "sql"),
    ("svelte", "svelte"),
    ("swift", "swift"),
    ("ts", "typescript"),
    ("tsx", "typescriptreact"),
    ("tex", "latex"),
    ("toml", "toml"),
    ("vue", "vue"),
    ("vim", "vim"),
    ("xml", "xml"),
    ("xsl", "xsl"),
    ("yaml", "yaml"),
    ("yml", "yaml"),
    ("zig", "zig"),
    ("zon", "zig"),
    ("zsh", "bash"),
    ("c++", "cpp"),
    ("erl", "erlang"),
    ("exs", "elixir"),
    ("ex", "elixir"),
    ("elm", "elm"),
    ("fsharp", "fsharp"),
    ("fs", "fsharp"),
    ("fsx", "fsharp"),
    ("fsscript", "fsharp"),
    ("fsi", "fsharp"),
    ("java", "java"),
    ("css", "css"),
];

/// OpenTUI's file name table (lower case).
const NAMES: &[(&str, &str)] = &[
    (".bash_aliases", "bash"),
    (".bash_logout", "bash"),
    (".bash_profile", "bash"),
    (".bashrc", "bash"),
    (".kshrc", "bash"),
    (".profile", "bash"),
    (".vimrc", "vim"),
    (".zlogin", "bash"),
    (".zlogout", "bash"),
    (".zprofile", "bash"),
    (".zshenv", "bash"),
    (".zshrc", "bash"),
    ("appfile", "ruby"),
    ("berksfile", "ruby"),
    ("brewfile", "ruby"),
    ("cheffile", "ruby"),
    ("containerfile", "dockerfile"),
    ("dockerfile", "dockerfile"),
    ("fastfile", "ruby"),
    ("gemfile", "ruby"),
    ("gnumakefile", "make"),
    ("gvimrc", "vim"),
    ("guardfile", "ruby"),
    ("makefile", "make"),
    ("podfile", "ruby"),
    ("rakefile", "ruby"),
    ("thorfile", "ruby"),
    ("vagrantfile", "ruby"),
];

fn lookup(table: &[(&'static str, &'static str)], key: &str) -> Option<&'static str> {
    table.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
}

/// A code fence's info string to a filetype, as OpenTUI does it: first word, lower case, a
/// leading dot dropped, then the file name table, the extension table, and finally the word
/// itself. `rust,no_run` is therefore not Rust there either.
pub fn filetype_for_info(info: &str) -> Option<String> {
    let word = info.split_whitespace().next()?;
    let lower = word.to_ascii_lowercase();
    if let Some(v) = lookup(NAMES, &lower) {
        return Some(v.to_string());
    }
    let key = lower.trim_start_matches('.');
    if key.is_empty() {
        return None;
    }
    Some(
        lookup(NAMES, key)
            .or_else(|| ext_of(key))
            .or_else(|| lookup(EXT, key))
            .map_or_else(|| key.to_string(), str::to_string),
    )
}

/// `foo.rs`-shaped keys resolve through the file name's extension.
fn ext_of(key: &str) -> Option<&'static str> {
    let base = key.rsplit('/').next().unwrap_or(key);
    if let Some(v) = lookup(NAMES, base) {
        return Some(v);
    }
    let dot = base.rfind('.')?;
    if dot + 1 == base.len() {
        return None;
    }
    lookup(EXT, &base[dot + 1..])
}

/// opencode's own extension table for file views (`filetype()` in its util), which is a
/// different, VS Code flavoured list from OpenTUI's. JavaScript and JSX files are drawn with
/// the TypeScript parser.
pub fn filetype_for_path(path: &str) -> Option<&'static str> {
    let file = path.rsplit('/').next().unwrap_or(path);
    // node's `path.extname`: the last dot that is not the first character
    let ext = match file.rfind('.') {
        Some(i) if i > 0 => &file[i..],
        _ => "",
    };
    let mapped = lookup(PATH_EXT, ext)?;
    Some(match mapped {
        "typescriptreact" | "javascriptreact" | "javascript" => "typescript",
        m => m,
    })
}

/// opencode's extension table (`Zy` in its util chunk), keyed with the leading dot.
const PATH_EXT: &[(&str, &str)] = &[
    (".abap", "abap"),
    (".bat", "bat"),
    (".bib", "bibtex"),
    (".bibtex", "bibtex"),
    (".clj", "clojure"),
    (".cljs", "clojure"),
    (".cljc", "clojure"),
    (".edn", "clojure"),
    (".coffee", "coffeescript"),
    (".c", "c"),
    (".cpp", "cpp"),
    (".cxx", "cpp"),
    (".cc", "cpp"),
    (".c++", "cpp"),
    (".cs", "csharp"),
    (".csx", "csharp"),
    (".css", "css"),
    (".d", "d"),
    (".pas", "pascal"),
    (".pascal", "pascal"),
    (".diff", "diff"),
    (".patch", "diff"),
    (".dart", "dart"),
    (".dockerfile", "dockerfile"),
    (".ex", "elixir"),
    (".exs", "elixir"),
    (".erl", "erlang"),
    (".ets", "typescript"),
    (".hrl", "erlang"),
    (".fs", "fsharp"),
    (".fsi", "fsharp"),
    (".fsx", "fsharp"),
    (".fsscript", "fsharp"),
    (".gitcommit", "git-commit"),
    (".gitrebase", "git-rebase"),
    (".go", "go"),
    (".groovy", "groovy"),
    (".gleam", "gleam"),
    (".hbs", "handlebars"),
    (".handlebars", "handlebars"),
    (".hs", "haskell"),
    (".lhs", "haskell"),
    (".html", "html"),
    (".htm", "html"),
    (".ini", "ini"),
    (".java", "java"),
    (".jl", "julia"),
    (".js", "javascript"),
    (".kt", "kotlin"),
    (".kts", "kotlin"),
    (".jsx", "javascriptreact"),
    (".json", "json"),
    (".tex", "latex"),
    (".latex", "latex"),
    (".less", "less"),
    (".lua", "lua"),
    (".makefile", "makefile"),
    ("makefile", "makefile"),
    (".md", "markdown"),
    (".markdown", "markdown"),
    (".m", "objective-c"),
    (".mm", "objective-cpp"),
    (".pl", "perl"),
    (".pm", "perl"),
    (".pm6", "perl6"),
    (".php", "php"),
    (".ps1", "powershell"),
    (".psm1", "powershell"),
    (".pug", "jade"),
    (".jade", "jade"),
    (".py", "python"),
    (".r", "r"),
    (".cshtml", "razor"),
    (".razor", "razor"),
    (".rb", "ruby"),
    (".rake", "ruby"),
    (".gemspec", "ruby"),
    (".ru", "ruby"),
    (".erb", "erb"),
    (".html.erb", "erb"),
    (".js.erb", "erb"),
    (".css.erb", "erb"),
    (".json.erb", "erb"),
    (".rs", "rust"),
    (".scss", "scss"),
    (".sass", "sass"),
    (".scala", "scala"),
    (".shader", "shaderlab"),
    (".sh", "shellscript"),
    (".bash", "shellscript"),
    (".zsh", "shellscript"),
    (".ksh", "shellscript"),
    (".sql", "sql"),
    (".svelte", "svelte"),
    (".swift", "swift"),
    (".ts", "typescript"),
    (".tsx", "typescriptreact"),
    (".mts", "typescript"),
    (".cts", "typescript"),
    (".mtsx", "typescriptreact"),
    (".ctsx", "typescriptreact"),
    (".xml", "xml"),
    (".xsl", "xsl"),
    (".yaml", "yaml"),
    (".yml", "yaml"),
    (".mjs", "javascript"),
    (".cjs", "javascript"),
    (".vue", "vue"),
    (".zig", "zig"),
    (".zon", "zig"),
    (".astro", "astro"),
    (".ml", "ocaml"),
    (".mli", "ocaml"),
    (".tf", "terraform"),
    (".tfvars", "terraform-vars"),
    (".hcl", "hcl"),
    (".nix", "nix"),
    (".typ", "typst"),
    (".typc", "typst"),
];

// ---- highlighting ---------------------------------------------------------------------------

/// `(start byte, end byte, capture index)` in the order the query cursor produced them, sorted
/// stably by start.
type Marks = Arc<Vec<(usize, usize, u32)>>;

/// Run the query over `code`. Cached by (grammar, text), since a transcript is re-rendered on
/// every resize and scroll.
fn marks(lang: &'static Lang, code: &str) -> Option<Marks> {
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    let key = (lang.name, hash(code), code.len());
    let cache = CACHE.get_or_init(|| Mutex::new(Cache::default()));
    if let Some(m) = cache.lock().ok()?.get(&key) {
        return Some(m);
    }
    let mut parser = Parser::new();
    parser.set_language(&lang.language).ok()?;
    let tree = parser.parse(code, None)?;
    let mut cursor = QueryCursor::new();
    let mut caps = cursor.captures(&lang.query, tree.root_node(), code.as_bytes());
    let mut out: Vec<(usize, usize, u32)> = Vec::new();
    while let Some((m, i)) = caps.next() {
        let c = m.captures[*i];
        let n = c.node;
        // a node should never cut a character, but slicing the text must not be able to panic
        let mut start = n.start_byte().min(code.len());
        while !code.is_char_boundary(start) {
            start -= 1;
        }
        let mut end = n.end_byte().min(code.len());
        while !code.is_char_boundary(end) {
            end += 1;
        }
        out.push((start, end, c.index));
    }
    out.sort_by_key(|h| h.0); // stable, like JS `Array.prototype.sort`
    let out = Arc::new(out);
    if let Ok(mut c) = cache.lock() {
        c.put(key, out.clone());
    }
    Some(out)
}

fn hash(s: &str) -> u64 {
    // FNV-1a; collisions only cost a wrong cached highlight, and the length is part of the key
    let mut h = 0xcbf29ce484222325u64;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

type Key = (&'static str, u64, usize);

#[derive(Default)]
struct Cache {
    map: HashMap<Key, (u64, Marks)>,
    tick: u64,
    bytes: usize,
}

impl Cache {
    const MAX_ENTRIES: usize = 256;
    const MAX_BYTES: usize = 16 * 1024 * 1024;

    fn get(&mut self, k: &Key) -> Option<Marks> {
        self.tick += 1;
        let t = self.tick;
        self.map.get_mut(k).map(|e| {
            e.0 = t;
            e.1.clone()
        })
    }

    fn put(&mut self, k: Key, m: Marks) {
        self.tick += 1;
        self.bytes += m.len() * 24;
        self.map.insert(k, (self.tick, m));
        while self.map.len() > Self::MAX_ENTRIES || self.bytes > Self::MAX_BYTES {
            let Some(old) = self.map.iter().min_by_key(|(_, v)| v.0).map(|(k, _)| *k) else {
                break;
            };
            if let Some((_, m)) = self.map.remove(&old) {
                self.bytes = self.bytes.saturating_sub(m.len() * 24);
            }
        }
    }
}

#[derive(Clone, Copy, Default)]
struct Def2 {
    fg: Option<Color>,
    bg: Option<Color>,
    bold: Option<bool>,
    italic: Option<bool>,
    underline: Option<bool>,
}

fn tok(t: Tok, th: &Theme) -> Color {
    match t {
        Tok::Accent => th.accent,
        Tok::Background => th.background,
        Tok::DiffAdded => th.diff_added,
        Tok::DiffAddedBg => th.diff_added_bg,
        Tok::DiffContext => th.diff_context,
        Tok::DiffContextBg => th.diff_context_bg,
        Tok::DiffRemoved => th.diff_removed,
        Tok::DiffRemovedBg => th.diff_removed_bg,
        Tok::Error => th.error,
        Tok::Info => th.info,
        Tok::MarkdownBlockQuote => th.markdown_block_quote,
        Tok::MarkdownCode => th.markdown_code,
        Tok::MarkdownEmph => th.markdown_emph,
        Tok::MarkdownHeading => th.markdown_heading,
        Tok::MarkdownLink => th.markdown_link,
        Tok::MarkdownLinkText => th.markdown_link_text,
        Tok::MarkdownListItem => th.markdown_list_item,
        Tok::MarkdownStrong => th.markdown_strong,
        Tok::Secondary => th.secondary,
        Tok::Success => th.success,
        Tok::SyntaxComment => th.syntax_comment,
        Tok::SyntaxFunction => th.syntax_function,
        Tok::SyntaxKeyword => th.syntax_keyword,
        Tok::SyntaxNumber => th.syntax_number,
        Tok::SyntaxOperator => th.syntax_operator,
        Tok::SyntaxPunctuation => th.syntax_punctuation,
        Tok::SyntaxString => th.syntax_string,
        Tok::SyntaxType => th.syntax_type,
        Tok::SyntaxVariable => th.syntax_variable,
        Tok::Text => th.text,
        Tok::TextMuted => th.text_muted,
        Tok::Warning => th.warning,
    }
}

fn scope_table() -> &'static HashMap<&'static str, &'static Rule> {
    static T: OnceLock<HashMap<&'static str, &'static Rule>> = OnceLock::new();
    T.get_or_init(|| {
        let mut m = HashMap::new();
        for r in RULES {
            for s in r.scopes {
                m.insert(*s, r); // later rules replace earlier ones, as in `bJ`
            }
        }
        m
    })
}

/// `SyntaxStyle.getStyle`: the exact scope, else the part before the first dot.
fn style_of(group: &str, th: &Theme) -> Option<Def2> {
    let t = scope_table();
    let r = t.get(group).or_else(|| {
        group
            .contains('.')
            .then(|| group.split('.').next())
            .flatten()
            .and_then(|b| t.get(b))
    })?;
    Some(Def2 {
        fg: r.fg.map(|f| tok(f, th)),
        bg: r.bg.map(|b| tok(b, th)),
        bold: r.bold,
        italic: r.italic,
        underline: r.underline,
    })
}

fn to_style(d: Def2, th: &Theme) -> Style {
    let mut s = Style::new().fg(d.fg.unwrap_or(th.text));
    if let Some(bg) = d.bg {
        s = s.bg(bg);
    }
    if d.bold == Some(true) {
        s = s.add_modifier(Modifier::BOLD);
    }
    if d.italic == Some(true) {
        s = s.add_modifier(Modifier::ITALIC);
    }
    if d.underline == Some(true) {
        s = s.add_modifier(Modifier::UNDERLINED);
    }
    s
}

/// Port of OpenTUI's `treeSitterToTextChunks`: sweep the start and end of every capture, and
/// for each stretch of text with captures active, layer their styles shallowest scope first, then
/// in capture order. Returns `(text range, style)` pieces that tile `code`.
fn apply(
    code: &str,
    marks: &[(usize, usize, u32)],
    names: &[&str],
    th: &Theme,
) -> Vec<(usize, usize, Style)> {
    let default = to_style(Def2::default(), th);
    let mut events: Vec<(usize, bool, usize)> = Vec::with_capacity(marks.len() * 2);
    for (i, (s, e, _)) in marks.iter().enumerate() {
        if s == e {
            continue;
        }
        events.push((*s, true, i));
        events.push((*e, false, i));
    }
    // ends before starts at the same offset; stable otherwise
    events.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));

    // one style lookup per capture name
    let mut cache: HashMap<u32, Option<Def2>> = HashMap::new();
    let mut out: Vec<(usize, usize, Style)> = Vec::new();
    let mut active: BTreeSet<usize> = BTreeSet::new();
    let mut r = 0usize;
    let push = |a: usize, b: usize, s: Style, out: &mut Vec<(usize, usize, Style)>| {
        if a >= b {
            return;
        }
        match out.last_mut() {
            Some(l) if l.1 == a && l.2 == s => l.1 = b,
            _ => out.push((a, b, s)),
        }
    };
    for (off, is_start, hi) in events {
        if r < off && !active.is_empty() {
            let mut order: Vec<usize> = active.iter().copied().collect();
            order.sort_by_key(|&i| (names[marks[i].2 as usize].split('.').count(), i));
            let mut d = Def2::default();
            let mut any = false;
            for i in order {
                let cap = marks[i].2;
                let st = *cache
                    .entry(cap)
                    .or_insert_with(|| style_of(names[cap as usize], th));
                if let Some(st) = st {
                    any = true;
                    d.fg = st.fg.or(d.fg);
                    d.bg = st.bg.or(d.bg);
                    d.bold = st.bold.or(d.bold);
                    d.italic = st.italic.or(d.italic);
                    d.underline = st.underline.or(d.underline);
                }
            }
            let s = if any { to_style(d, th) } else { default };
            push(r, off, s, &mut out);
        } else if r < off {
            push(r, off, default, &mut out);
        }
        if is_start {
            active.insert(hi);
        } else {
            active.remove(&hi);
        }
        r = off;
    }
    push(r, code.len(), default, &mut out);
    out
}

/// Highlight `code` (no trailing newline) into one span list per line. `None` when the parse or
/// query could not run, so the caller can fall back.
pub fn highlight(lang: &'static Lang, code: &str, th: &Theme) -> Option<Vec<Vec<Span<'static>>>> {
    if code.len() > MAX_BYTES {
        return None;
    }
    let marks = marks(lang, code)?;
    let names: Vec<&str> = lang.query.capture_names().to_vec();
    let pieces = apply(code, &marks, &names, th);
    let mut lines: Vec<Vec<Span<'static>>> = vec![Vec::new()];
    for (a, b, style) in pieces {
        let mut first = true;
        for part in code[a..b].split('\n') {
            if !first {
                lines.push(Vec::new());
            }
            first = false;
            let part = part.strip_suffix('\r').unwrap_or(part);
            if part.is_empty() {
                continue;
            }
            let line = lines.last_mut().expect("one line exists");
            match line.last_mut() {
                Some(prev) if prev.style == style => prev.content.to_mut().push_str(part),
                _ => line.push(Span::styled(part.to_string(), style)),
            }
        }
    }
    Some(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_shipped_query_compiles_against_its_grammar() {
        for name in shipped() {
            assert!(lang(name).is_some(), "{name}");
        }
    }

    /// Rendering re-highlights every visible code block on each resize, so a cold parse has to
    /// be cheap and a repeat has to be a cache hit. The bounds are loose because tests run in a
    /// debug build on a busy box; in release a 400 line file is a few milliseconds cold.
    #[test]
    fn cold_and_warm_highlight_stay_within_a_frame_budget() {
        let th = Theme::builtin("opencode").unwrap();
        let code = "fn main() {\n    let name = std::env::args().nth(1).unwrap_or_else(|| \"w\".into());\n    println!(\"hello, {name}\");\n}\n"
            .repeat(100);
        let l = lang("rust").unwrap();
        let t0 = std::time::Instant::now();
        let cold = highlight(l, &code, &th).unwrap();
        let cold_t = t0.elapsed();
        let t1 = std::time::Instant::now();
        let warm = highlight(l, &code, &th).unwrap();
        let warm_t = t1.elapsed();
        eprintln!("400 lines of rust: cold {cold_t:?}, warm {warm_t:?}");
        assert_eq!(cold.len(), warm.len());
        assert!(cold_t.as_millis() < 1500, "cold {cold_t:?}");
        assert!(warm_t < cold_t, "warm {warm_t:?} cold {cold_t:?}");
        assert!(warm_t.as_millis() < 500, "warm {warm_t:?}");
        // past the cap a block is drawn plain instead of parsed
        let big = "let a = 1;\n".repeat(MAX_BYTES / 10 + 1);
        assert!(highlight(l, &big, &th).is_none());
    }

    #[test]
    fn info_strings_follow_opentui() {
        let f = |s: &str| filetype_for_info(s).unwrap();
        assert_eq!(f("sh"), "bash");
        assert_eq!(f("yml"), "yaml");
        assert_eq!(f("py"), "python");
        assert_eq!(f("Rust"), "rust");
        assert_eq!(f(".rs"), "rust");
        assert_eq!(f("tsx"), "typescriptreact");
        assert_eq!(f("shell"), "shell");
        assert_eq!(f("rust,no_run"), "rust,no_run");
        assert_eq!(f("python title=x"), "python");
        assert_eq!(f("Makefile"), "make");
        assert!(filetype_for_info("").is_none());
        assert_eq!(alias("typescriptreact"), "typescript");
    }

    #[test]
    fn paths_follow_opencodes_table_and_send_javascript_to_typescript() {
        assert_eq!(filetype_for_path("src/main.rs"), Some("rust"));
        assert_eq!(filetype_for_path("a/b.js"), Some("typescript"));
        assert_eq!(filetype_for_path("a/b.tsx"), Some("typescript"));
        assert_eq!(filetype_for_path("run.sh"), Some("shellscript"));
        assert_eq!(filetype_for_path("notes.txt"), None);
        assert_eq!(filetype_for_path("README"), None);
    }
}
