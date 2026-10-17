//! Pi's code highlighter: highlight.js 10.7.3 plus the token-to-colour mapping of Pi's
//! `buildCliHighlightTheme`, so a fenced block gets the colours Pi gives it, not the ones a
//! different grammar would.
//!
//! The grammars are not written out by hand. `tools/hljs-dump.mjs` compiles every language in
//! Pi's highlight.js and writes the result to `data/hljs.jsonl`, one language per line; `model`
//! reads a language from its line on first use, `jsre` turns the regex sources into Rust
//! patterns, and `engine` is `_highlight`. `tools/hljs-oracle.mjs` runs Pi's own code on the
//! same text and `tests/hljs_corpus.rs` requires identical runs.

mod engine;
mod jsre;
mod model;
#[cfg(test)]
mod tests;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::OnceLock;

use serde::Deserialize;

use crate::theme::Tok;
use engine::{Child, NodeRef};
use model::Language;

static DATA: &str = include_str!("../../../data/hljs.jsonl");

#[derive(Deserialize)]
struct Head {
    order: Vec<String>,
    aliases: HashMap<String, String>,
}

struct Index {
    head: Head,
    lines: HashMap<&'static str, &'static str>,
}

fn index() -> &'static Index {
    static INDEX: OnceLock<Index> = OnceLock::new();
    INDEX.get_or_init(|| {
        let mut it = DATA.lines();
        let head = serde_json::from_str(it.next().unwrap_or("{}")).expect("hljs.jsonl header");
        let lines = it.filter_map(|l| l.split_once('\t')).collect();
        Index { head, lines }
    })
}

thread_local! {
    static LOADED: RefCell<HashMap<String, Option<Rc<Language>>>> = RefCell::new(HashMap::new());
}

/// Language names in the order Pi registers them; auto-detection ties go to the earlier one.
pub fn order() -> &'static [String] {
    &index().head.order
}

/// `hljs.registerLanguage` keys only: no aliases, no lowercasing.
pub fn language_exists(name: &str) -> bool {
    index().lines.contains_key(name)
}

fn load(name: &str) -> Option<Rc<Language>> {
    if let Some(hit) = LOADED.with(|l| l.borrow().get(name).cloned()) {
        return hit;
    }
    let json = index().lines.get(name)?;
    let lang = Language::parse(name, json).ok().map(Rc::new);
    LOADED.with(|l| l.borrow_mut().insert(name.to_string(), lang.clone()));
    lang
}

/// `hljs.getLanguage`: lowercase, then a language name or one of its aliases.
pub fn get_language(name: &str) -> Option<Rc<Language>> {
    let canonical = index().head.aliases.get(&name.to_lowercase())?;
    load(canonical)
}

/// `supportsLanguage`.
pub fn supports(name: &str) -> bool {
    index().head.aliases.contains_key(&name.to_lowercase())
}

/// How Pi's theme draws a token: a colour, or one of the three text attributes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Fmt {
    Tok(Tok),
    Italic,
    Bold,
    Underline,
}

fn exact(scope: &str) -> Option<Fmt> {
    use Tok::*;
    Some(match scope {
        "keyword" | "name" => Fmt::Tok(SyntaxKeyword),
        "built_in" | "class" | "type" => Fmt::Tok(SyntaxType),
        "literal" | "number" => Fmt::Tok(SyntaxNumber),
        "regexp" | "string" => Fmt::Tok(SyntaxString),
        "comment" | "doctag" => Fmt::Tok(SyntaxComment),
        "meta" => Fmt::Tok(Muted),
        "function" | "title" => Fmt::Tok(SyntaxFunction),
        "tag" | "punctuation" => Fmt::Tok(SyntaxPunctuation),
        "attr" | "variable" | "params" => Fmt::Tok(SyntaxVariable),
        "operator" => Fmt::Tok(SyntaxOperator),
        "emphasis" => Fmt::Italic,
        "strong" => Fmt::Bold,
        "link" => Fmt::Underline,
        "addition" => Fmt::Tok(ToolDiffAdded),
        "deletion" => Fmt::Tok(ToolDiffRemoved),
        _ => return None,
    })
}

/// `getScopeFormatter`: the scope itself, then what is before its first `.`, then before its
/// first `-`.
fn formatter(scope: &str) -> Option<Fmt> {
    exact(scope)
        .or_else(|| scope.split_once('.').and_then(|(p, _)| exact(p)))
        .or_else(|| scope.split_once('-').and_then(|(p, _)| exact(p)))
}

/// A run of text and how Pi would draw it (`None` is the default style).
pub type Run = (String, Option<Fmt>);

/// The node tree as `renderHighlightedHtml` reads its markup: a span with an `hljs-` class
/// pushes a scope, a sublanguage span (no prefix) pushes nothing a formatter can match, and
/// text takes the innermost scope that has a formatter. Text is flushed as one run at every
/// span boundary, not merged across them, because Pi wraps each flushed run in its own escape
/// pair and that decides which rows keep a colour (see `highlight_rows`). A node with no kind
/// emits no span, so the text around it is still one run.
struct Walker {
    stack: Vec<Option<Fmt>>,
    pending: String,
    out: Vec<Run>,
}

impl Walker {
    fn flush(&mut self) {
        if !self.pending.is_empty() {
            let f = self.stack.iter().rev().find_map(|f| *f);
            self.out.push((std::mem::take(&mut self.pending), f));
        }
    }

    fn walk(&mut self, node: &NodeRef) {
        for child in &node.borrow().children {
            match child {
                Child::Text(t) => self.pending.push_str(t),
                Child::Node(n) => {
                    let (kind, sub) = {
                        let b = n.borrow();
                        (b.kind.clone().filter(|k| !k.is_empty()), b.sublanguage)
                    };
                    let Some(k) = kind else {
                        self.walk(n);
                        continue;
                    };
                    // only the first word of a class list carries the `hljs-` prefix
                    let f = if sub {
                        None
                    } else {
                        k.split_whitespace().next().and_then(formatter)
                    };
                    self.flush();
                    self.stack.push(f);
                    self.walk(n);
                    self.flush();
                    self.stack.pop();
                }
            }
        }
    }
}

/// Highlight `code` as Pi does and split it into rows. `None` when Pi has no grammar for
/// `lang`.
///
/// Pi wraps each token in one on/off escape pair, splits the result at `\n`, and the terminal
/// resets after every row, so a token that spans rows keeps its style on its first row only.
pub fn highlight_rows(lang: &str, code: &str) -> Option<Vec<Vec<Run>>> {
    let language = get_language(lang)?;
    let result = engine::highlight_run(&language, &language.name, code, true, None);
    let mut runs: Vec<Run> = Vec::new();
    if result.errored {
        runs.push((code.to_string(), None));
    } else {
        let mut w = Walker {
            stack: Vec::new(),
            pending: String::new(),
            out: Vec::new(),
        };
        w.walk(&result.emitter.root);
        w.flush();
        runs = w.out;
    }
    let mut rows: Vec<Vec<Run>> = vec![Vec::new()];
    for (text, f) in runs {
        for (i, piece) in text.split('\n').enumerate() {
            if i > 0 {
                rows.push(Vec::new());
            }
            if piece.is_empty() {
                continue;
            }
            let f = if i == 0 { f } else { None };
            let row = rows.last_mut().expect("rows is never empty");
            match row.last_mut() {
                Some((last, lf)) if *lf == f => last.push_str(piece),
                _ => row.push((piece.to_string(), f)),
            }
        }
    }
    Some(rows)
}

/// Every language, parsed: for the test that walks all grammars.
#[cfg(test)]
pub(crate) fn all_languages() -> Vec<Rc<Language>> {
    order()
        .iter()
        .map(|n| {
            let json = index().lines.get(n.as_str()).expect("line for every name");
            Rc::new(Language::parse(n, json).unwrap_or_else(|e| panic!("{e}")))
        })
        .collect()
}
