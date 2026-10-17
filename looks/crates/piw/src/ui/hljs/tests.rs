//! Engine-level checks that need the internals. The colours themselves are compared against
//! Pi in `crates/piw/tests/hljs_corpus.rs`.

use super::jsre::Re;
use super::*;

/// Every regex source in every grammar must translate and compile, or a rule would silently
/// never match.
#[test]
fn every_regex_in_every_grammar_compiles() {
    let langs = all_languages();
    assert_eq!(langs.len(), 191);
    let mut bad = Vec::new();
    let mut n = 0;
    for l in &langs {
        let ci = l.case_insensitive;
        for (i, m) in l.modes.iter().enumerate() {
            let mut check = |what: &str, src: &str, anchored: bool| {
                n += 1;
                if let Err(e) = Re::new(src, ci, anchored, true) {
                    bad.push(format!("{} mode {i} {what}: {e}", l.name));
                }
            };
            if m.has_begin {
                check("begin", m.begin.source(), false);
            }
            if let Some(e) = &m.end {
                check("end", e, true);
            }
            if let Some(t) = &m.term {
                check("terminator", t.source(), false);
            }
            if let Some(x) = &m.illegal {
                check("illegal", x.source(), false);
            }
            if let Some(k) = &m.kw_pattern {
                check("keyword pattern", k.source(), false);
            }
        }
    }
    assert!(
        bad.is_empty(),
        "{} of {n} regexes failed:\n{}",
        bad.len(),
        bad.join("\n")
    );
}

fn matches(src: &str, ci: bool, text: &str) -> Option<(usize, usize)> {
    let re = Re::new(src, ci, false, false).unwrap_or_else(|e| panic!("{src}: {e}"));
    re.find_at(text, 0).map(|h| (h.start, h.end))
}

#[test]
fn javascript_regex_quirks_become_the_same_matches() {
    // `\w`, `\d` and `\b` see ASCII only
    assert_eq!(matches(r"\w+", false, "é abc"), Some((3, 6)));
    assert_eq!(matches(r"\bé", false, "aé"), Some((1, 3)));
    assert_eq!(matches(r"\d", false, "٣4"), Some((2, 3)));
    // `.` stops at line terminators, `[^]` does not
    assert_eq!(
        matches(r"a.b", false, "a\nb a\u{2028}b a-b"),
        Some((10, 13))
    );
    assert_eq!(matches(r"a[^]b", false, "a\nb"), Some((0, 3)));
    // a brace that is not a quantifier, a bare `]`, `\/`, and `[` inside a class are literals
    assert_eq!(matches(r"a{b}]", false, "xa{b}]"), Some((1, 6)));
    assert_eq!(matches(r"\/[[]", false, "/["), Some((0, 2)));
    assert_eq!(matches(r"x{2}", false, "xxx"), Some((0, 2)));
    // `&&`, `~~` and `--` are set operators in Rust and plain characters here
    assert_eq!(matches(r"[&&]+", false, "a&&"), Some((1, 3)));
    assert_eq!(matches(r"[a\-z]+", false, "q-az"), Some((1, 4)));
    assert_eq!(matches(r"[\w-]+", false, "ab-c d"), Some((0, 4)));
    // `\s` is JavaScript's set, and `\uXXXX` and `\xHH` are escapes
    assert_eq!(matches(r"\s", false, "a\u{feff}"), Some((1, 4)));
    assert_eq!(matches(r"é\x41", false, "éA"), Some((0, 3)));
    // look-ahead and back-references
    assert_eq!(matches(r"a(?=b)", false, "ac ab"), Some((3, 4)));
    assert_eq!(matches(r"(['\x22])x\1", false, "'x\" 'x'"), Some((4, 7)));
    // named groups are numbered, `\k` finds them, and `i` folds case
    assert_eq!(matches(r"(?<q>[ab])\k<q>", true, "xAa"), Some((1, 3)));
    // `$` and `^` match at line boundaries
    assert_eq!(matches(r"^b$", false, "a\nb\nc"), Some((2, 3)));
}

#[test]
fn rows_keep_their_text_and_language_lookup_follows_hljs() {
    let rows = highlight_rows("json", "{\"a\": [1, null]}").unwrap();
    assert_eq!(rows.len(), 1);
    let text: String = rows[0].iter().map(|(t, _)| t.as_str()).collect();
    assert_eq!(text, "{\"a\": [1, null]}");
    assert!(highlight_rows("nosuchlang", "x").is_none());
    assert!(supports("TS") && supports("sh") && supports("text") && !supports("rust2"));
}
