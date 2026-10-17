//! Cell-diff test for the tree-sitter highlighter: every fenced block that real opencode 1.18.34
//! drew in `reference/critics/openw-r1/fidelity` (extracted by `tools/ts-ref-extract.py`) must
//! come out of `tuikit::syntax` with the same colours, background and weight on every character.

use ratatui::style::{Color, Modifier};
use serde_json::Value;
use tuikit::syntax::{highlight_token, set_opencode_parity};
use tuikit::Theme;

fn hex(c: Option<Color>) -> String {
    match c {
        Some(Color::Rgb(r, g, b)) => format!("#{r:02x}{g:02x}{b:02x}"),
        other => panic!("not an rgb colour: {other:?}"),
    }
}

#[test]
fn fences_match_what_opencode_drew() {
    set_opencode_parity(true);
    let theme = Theme::builtin("opencode").unwrap();
    let data: Vec<Value> =
        serde_json::from_str(include_str!("data/ts_ref.json")).expect("ts_ref.json");
    assert!(data.len() >= 15, "{} fences", data.len());
    let mut bad = Vec::new();
    for fence in &data {
        let lang = fence["lang"].as_str().unwrap();
        let code = fence["code"].as_str().unwrap();
        let lines = highlight_token(lang, code, &theme);
        let want = fence["runs"].as_array().unwrap();
        assert_eq!(lines.len(), want.len(), "{lang}: line count");
        for (i, (line, runs)) in lines.iter().zip(want).enumerate() {
            // per character: (char, fg, bg, bold)
            let mut got: Vec<(char, String, String, bool)> = Vec::new();
            for span in line {
                let fg = hex(span.style.fg);
                let bg = span.style.bg.map(|c| hex(Some(c))).unwrap_or_default();
                let bold = span.style.add_modifier.contains(Modifier::BOLD);
                got.extend(
                    span.content
                        .chars()
                        .map(|c| (c, fg.clone(), bg.clone(), bold)),
                );
            }
            let mut exp: Vec<(char, String, String, bool)> = Vec::new();
            for r in runs.as_array().unwrap() {
                let r = r.as_array().unwrap();
                let (fg, bg, bold) = (
                    r[1].as_str().unwrap(),
                    r[2].as_str().unwrap(),
                    r[3].as_bool().unwrap(),
                );
                exp.extend(
                    r[0].as_str()
                        .unwrap()
                        .chars()
                        .map(|c| (c, fg.to_string(), bg.to_string(), bold)),
                );
            }
            // the capture only knows a space's background, not its foreground
            for (g, e) in got.iter().zip(&exp) {
                let same = g.0 == e.0
                    && (g.0 == ' ' || g.1 == e.1)
                    && g.2 == e.2
                    && (g.0 == ' ' || g.3 == e.3);
                if !same {
                    bad.push(format!("{lang} line {i}: {g:?} want {e:?}"));
                }
            }
            assert_eq!(got.len(), exp.len(), "{lang} line {i} width");
        }
    }
    assert!(
        bad.is_empty(),
        "{} cells differ:\n{}",
        bad.len(),
        bad.join("\n")
    );
}

/// opencode configures a swift parser but 1.18.34 draws swift fences plain
/// (`tools/critic/fid/f04_langs.py`), as it does languages it has no parser for.
#[test]
fn swift_and_unparsed_languages_stay_plain() {
    set_opencode_parity(true);
    let theme = Theme::builtin("opencode").unwrap();
    for lang in ["swift", "sql", "shell", "console", "klingon"] {
        let lines = highlight_token(lang, "import Foundation\nlet x = 1 // c\n", &theme);
        assert!(
            lines
                .iter()
                .flatten()
                .all(|s| s.style.fg == Some(theme.text) && s.style.bg.is_none()),
            "{lang}"
        );
    }
}
