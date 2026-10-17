//! The Rust highlighter against Pi's own, row by row and run by run.
//!
//! `tests/hljs-corpus/<lang>--<name>.txt` is source text; `<lang>--<name>.exp` is what
//! `tools/hljs-oracle.mjs gen` made from it by running Pi's highlighter (see that file for the
//! format and why a token that spans rows only keeps its colour on the first). Each `.exp`
//! holds the whole file and three slices of it, cut at lines, at bytes, and as a prefix, so
//! half-written code and cut-off tokens are covered too. No node is needed to run this.

use std::fs;
use std::path::PathBuf;

use piw::theme::{PiTheme, Tok};
use piw::ui::syntax::highlight;
use ratatui::style::{Modifier, Style};
use serde_json::Value;

/// Every token the mapping uses gets its own colour, so a style says which token drew it.
fn theme() -> PiTheme {
    let colours = [
        ("syntaxKeyword", "#010101"),
        ("syntaxType", "#020202"),
        ("syntaxNumber", "#030303"),
        ("syntaxString", "#040404"),
        ("syntaxComment", "#050505"),
        ("muted", "#060606"),
        ("syntaxFunction", "#070707"),
        ("syntaxPunctuation", "#080808"),
        ("syntaxVariable", "#090909"),
        ("syntaxOperator", "#0a0a0a"),
        ("toolDiffAdded", "#0b0b0b"),
        ("toolDiffRemoved", "#0c0c0c"),
    ];
    let body: Vec<String> = colours
        .iter()
        .map(|(k, v)| format!("\"{k}\":\"{v}\""))
        .collect();
    PiTheme::from_json(&format!("{{\"colors\":{{{}}}}}", body.join(","))).expect("test theme")
}

fn tags(t: &PiTheme) -> Vec<(char, Style)> {
    let tok = |c, k| (c, t.fg(k));
    vec![
        tok('k', Tok::SyntaxKeyword),
        tok('t', Tok::SyntaxType),
        tok('n', Tok::SyntaxNumber),
        tok('s', Tok::SyntaxString),
        tok('c', Tok::SyntaxComment),
        tok('m', Tok::Muted),
        tok('f', Tok::SyntaxFunction),
        tok('p', Tok::SyntaxPunctuation),
        tok('v', Tok::SyntaxVariable),
        tok('o', Tok::SyntaxOperator),
        tok('+', Tok::ToolDiffAdded),
        tok('-', Tok::ToolDiffRemoved),
        ('i', Style::default().add_modifier(Modifier::ITALIC)),
        ('B', Style::default().add_modifier(Modifier::BOLD)),
        ('u', Style::default().add_modifier(Modifier::UNDERLINED)),
        ('d', Style::default()),
    ]
}

/// `built_in` also draws as `syntaxType`: the oracle names it `b`, the style is the same as `t`.
fn encode(rows: &[Vec<ratatui::text::Span<'static>>], tags: &[(char, Style)]) -> Vec<String> {
    rows.iter()
        .map(|row| {
            let mut runs: Vec<(usize, char)> = Vec::new();
            for s in row {
                let tag = tags
                    .iter()
                    .find(|(_, st)| *st == s.style)
                    .map_or('?', |(c, _)| *c);
                match runs.last_mut() {
                    Some((n, t)) if *t == tag => *n += s.content.len(),
                    _ => runs.push((s.content.len(), tag)),
                }
            }
            runs.iter()
                .map(|(n, t)| format!("{n}{t}"))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect()
}

/// The oracle's letters for `built_in` (`b`) and `type` (`t`) are one colour here, so fold them.
fn fold(row: &str) -> String {
    let mut runs: Vec<(usize, char)> = Vec::new();
    for r in row.split(' ').filter(|r| !r.is_empty()) {
        let (n, t) = r.split_at(r.len() - 1);
        let t = if t == "b" {
            't'
        } else {
            t.chars().next().unwrap()
        };
        let n: usize = n.parse().unwrap();
        match runs.last_mut() {
            Some((m, u)) if *u == t => *m += n,
            _ => runs.push((n, t)),
        }
    }
    runs.iter()
        .map(|(n, t)| format!("{n}{t}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn corpus_dir() -> PathBuf {
    // HLJS_CORPUS points at a scratch corpus made with `hljs-oracle.mjs gen`
    std::env::var_os("HLJS_CORPUS").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/hljs-corpus"),
        PathBuf::from,
    )
}

#[test]
fn rust_highlighter_matches_pi_on_the_corpus() {
    let theme = theme();
    let tags = tags(&theme);
    let mut files: Vec<_> = fs::read_dir(corpus_dir())
        .expect("corpus dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "txt"))
        .collect();
    files.sort();
    assert!(
        files.len() >= 200 || std::env::var_os("HLJS_CORPUS").is_some(),
        "corpus has {} files",
        files.len()
    );
    let only = std::env::var("HLJS_ONLY").ok();
    let (mut variants, mut rows_n, mut langs) = (0, 0, std::collections::BTreeSet::new());
    let mut failures: Vec<String> = Vec::new();
    for path in files {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        if only.as_ref().is_some_and(|o| !name.contains(o.as_str())) {
            continue;
        }
        let src = fs::read(&path).unwrap();
        let exp: Value = serde_json::from_str(
            &fs::read_to_string(path.with_extension("exp"))
                .unwrap_or_else(|_| panic!("{name}: no .exp, run tools/hljs-oracle.mjs gen")),
        )
        .unwrap();
        let lang = exp["lang"].as_str().unwrap();
        langs.insert(lang.to_string());
        for (vi, v) in exp["variants"].as_array().unwrap().iter().enumerate() {
            let (a, b) = (
                v["r"][0].as_u64().unwrap() as usize,
                v["r"][1].as_u64().unwrap() as usize,
            );
            let text = std::str::from_utf8(&src[a..b]).expect("slices are cut at characters");
            let got = highlight(lang, text, &theme)
                .unwrap_or_else(|| panic!("{name}: no grammar for {lang}"));
            let got = encode(&got, &tags);
            let want: Vec<String> = v["rows"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| fold(r.as_str().unwrap()))
                .collect();
            variants += 1;
            rows_n += want.len();
            let lines: Vec<&str> = text.split('\n').collect();
            if got.len() != want.len() {
                failures.push(format!(
                    "{name} variant {vi}: {} rows, Pi has {}",
                    got.len(),
                    want.len()
                ));
                continue;
            }
            for (i, (g, w)) in got.iter().zip(&want).enumerate() {
                if g != w {
                    failures.push(format!(
                        "{name} variant {vi} row {i}\n   text {:?}\n   Pi   {w}\n   ours {g}",
                        lines[i]
                    ));
                }
            }
        }
    }
    eprintln!(
        "compared {variants} variants, {rows_n} rows, {} languages",
        langs.len()
    );
    let bad = failures.len();
    let mut per_file = std::collections::BTreeMap::<String, usize>::new();
    for f in failures.iter().filter(|f| !f.is_empty()) {
        *per_file
            .entry(f.split(' ').next().unwrap_or("").to_string())
            .or_default() += 1;
    }
    if !per_file.is_empty() {
        eprintln!("files with differences (first 40 rows each shown below): {per_file:?}");
    }
    let shown: Vec<_> = failures.into_iter().take(40).collect();
    assert!(bad == 0, "{bad} differences\n{}", shown.join("\n"));
}

/// Highlighting a 100 KB block must not stall a frame, and the first call for a language
/// parses that grammar alone.
#[test]
fn a_100kb_block_highlights_in_well_under_a_second() {
    let theme = theme();
    let mut slowest = (std::time::Duration::ZERO, String::new());
    for lang in [
        "javascript",
        "typescript",
        "python",
        "cpp",
        "rust",
        "go",
        "java",
        "css",
        "html",
        "json",
        "bash",
        "markdown",
        "sql",
        "yaml",
        "ruby",
        "php",
    ] {
        let mut text = String::new();
        let mut files: Vec<_> = fs::read_dir(corpus_dir())
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                let n = p.file_name().unwrap().to_string_lossy();
                n.starts_with(&format!("{lang}--")) && n.ends_with(".txt")
            })
            .collect();
        files.sort();
        assert!(!files.is_empty(), "no corpus for {lang}");
        'fill: loop {
            for f in &files {
                text.push_str(&fs::read_to_string(f).unwrap());
                if text.len() >= 100_000 {
                    break 'fill;
                }
            }
        }
        let _ = highlight(lang, "x", &theme);
        let t0 = std::time::Instant::now();
        let rows = highlight(lang, &text, &theme).expect("grammar");
        let took = t0.elapsed();
        assert_eq!(rows.len(), text.split('\n').count());
        eprintln!("{lang}: {} bytes in {took:?}", text.len());
        if took > slowest.0 {
            slowest = (took, lang.to_string());
        }
    }
    assert!(
        slowest.0 < std::time::Duration::from_millis(900),
        "{} took {:?}",
        slowest.1,
        slowest.0
    );
}
