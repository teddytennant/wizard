//! Text helpers shared by the tool renderers: OpenTUI-style wrapping, opencode's path and
//! duration formatting, output cleaning and `collapseToolOutput`.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde_json::Value;
use tuikit::{ansi, width};
use unicode_width::UnicodeWidthChar;

fn cw(c: char) -> usize {
    c.width().unwrap_or(0)
}

/// Wrap like OpenTUI's `<text>`: break at spaces and after `-` or `/`, hard-break a token that
/// is wider than the row. Trailing spaces hang off the row and are dropped. `\n` starts a new
/// paragraph; an empty paragraph is an empty row. Leading indentation of a paragraph stays.
pub fn wrap_text(s: &str, w: usize) -> Vec<String> {
    let w = w.max(1);
    let mut out = Vec::new();
    for para in s.split('\n') {
        wrap_para(para, w, &mut out);
    }
    out
}

fn wrap_para(para: &str, w: usize, out: &mut Vec<String>) {
    if para.is_empty() {
        out.push(String::new());
        return;
    }
    let indent: String = para.chars().take_while(|c| *c == ' ').collect();
    let rest = &para[indent.len()..];
    if rest.is_empty() {
        out.push(indent);
        return;
    }
    // Atoms: a run of spaces, or a word that ends at `-` / `/` or at the next space.
    let mut atoms: Vec<(String, bool)> = Vec::new(); // (text, is_space)
    let mut cur = String::new();
    let mut cur_space = false;
    for c in rest.chars() {
        let sp = c == ' ';
        if !cur.is_empty() && sp != cur_space {
            atoms.push((std::mem::take(&mut cur), cur_space));
        }
        cur_space = sp;
        cur.push(c);
        if !sp && (c == '-' || c == '/') {
            atoms.push((std::mem::take(&mut cur), false));
        }
    }
    if !cur.is_empty() {
        atoms.push((cur, cur_space));
    }

    let mut lw: usize = indent.chars().map(cw).sum();
    let mut line = indent;
    let mut hung = String::new();
    let mut fresh = true; // no word on this row yet
    for (text, is_space) in atoms {
        if is_space {
            if !fresh {
                hung.push_str(&text);
            }
            continue;
        }
        let tw: usize = text.chars().map(cw).sum();
        let hw: usize = hung.chars().map(cw).sum();
        if !fresh && lw + hw + tw > w {
            out.push(std::mem::take(&mut line));
            lw = 0;
            hung.clear();
            fresh = true;
        }
        if !fresh {
            line.push_str(&hung);
            lw += hw;
        }
        hung.clear();
        fresh = false;
        if lw + tw <= w {
            line.push_str(&text);
            lw += tw;
            continue;
        }
        // Wider than a whole row: break it by cells.
        for c in text.chars() {
            let c_w = cw(c);
            if lw + c_w > w && lw > 0 {
                out.push(std::mem::take(&mut line));
                lw = 0;
            }
            line.push(c);
            lw += c_w;
        }
    }
    out.push(line);
}

/// opencode's `usePathFormatter().format`: relative to the working directory when inside it,
/// `~`-abbreviated otherwise, `.` for the directory itself, empty for empty input.
pub fn fmt_path(p: &str, cwd: &str) -> String {
    if p.is_empty() {
        return String::new();
    }
    let abs = normalize(&if p.starts_with('/') || cwd.is_empty() {
        p.to_string()
    } else {
        format!("{}/{p}", cwd.trim_end_matches('/'))
    });
    if !cwd.is_empty() {
        let base = normalize(cwd);
        if abs == base {
            return ".".into();
        }
        let prefix = if base == "/" {
            "/".to_string()
        } else {
            format!("{base}/")
        };
        if let Some(rest) = abs.strip_prefix(&prefix) {
            return rest.to_string();
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        let home = home.trim_end_matches('/');
        if abs == home {
            return "~".into();
        }
        if let Some(rest) = abs.strip_prefix(&format!("{home}/")) {
            return format!("~/{rest}");
        }
    }
    abs
}

/// Lexical `path.resolve`: drop `.` and empty segments, fold `..`.
fn normalize(p: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    format!("/{}", parts.join("/"))
}

pub fn titlecase(s: &str) -> String {
    let mut out = String::new();
    let mut start = true;
    for ch in s.chars() {
        if start && ch.is_alphanumeric() {
            out.extend(ch.to_uppercase());
            start = false;
        } else {
            out.push(ch);
            start = !(ch.is_alphanumeric() || ch == '\'' || ch == '_');
        }
    }
    out
}

/// First non-empty string among `keys`.
pub fn str_in<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|k| v.get(*k).and_then(Value::as_str))
        .filter(|s| !s.is_empty())
}

pub fn num_in(v: &Value, keys: &[&str]) -> Option<i64> {
    keys.iter().find_map(|k| v.get(*k).and_then(Value::as_i64))
}

/// opencode's `input()`: primitive arguments as `[k=v, ...]`, in the order the call had them.
pub fn input_args(v: &Value, omit: &[&str]) -> String {
    let Some(map) = v.as_object() else {
        return String::new();
    };
    let parts: Vec<String> = map
        .iter()
        .filter(|(k, _)| !omit.contains(&k.as_str()))
        .filter_map(|(k, v)| match v {
            Value::String(s) => Some(format!("{k}={s}")),
            Value::Number(n) => Some(format!("{k}={n}")),
            Value::Bool(b) => Some(format!("{k}={b}")),
            _ => None,
        })
        .collect();
    if parts.is_empty() {
        String::new()
    } else {
        format!("[{}]", parts.join(", "))
    }
}

/// Tool output as opencode prints it: escape sequences gone, newlines normalised, control
/// bytes dropped, then `trim()` of the whole string.
pub fn clean_output(s: &str) -> String {
    let s = ansi::strip(s);
    let s = width::normalize_newlines(&s);
    let s: Cow<str> = width::sanitize(&s);
    s.trim().to_string()
}

/// opencode's `collapseToolOutput`; the bool is `overflow`.
pub fn collapse(output: &str, max_lines: usize, max_chars: usize) -> (String, bool) {
    let lines: Vec<&str> = output.split('\n').collect();
    if lines.len() <= max_lines && output.chars().count() <= max_chars {
        return (output.to_string(), false);
    }
    let preview = lines[..lines.len().min(max_lines)].join("\n");
    if preview.chars().count() > max_chars {
        let cut: String = preview.chars().take(max_chars.saturating_sub(1)).collect();
        return (cut + "…", true);
    }
    let mut v: Vec<&str> = lines[..lines.len().min(max_lines)].to_vec();
    v.push("…");
    (v.join("\n"), true)
}

// ---- how long a call took ------------------------------------------------------------------

thread_local! {
    static CLOCK: RefCell<HashMap<String, (Instant, Option<Duration>)>> = RefCell::new(HashMap::new());
}

/// Forget every remembered call. The map is keyed by call id and only grows, so it is emptied
/// whenever the transcript is replaced.
pub fn reset_clock() {
    CLOCK.with(|c| c.borrow_mut().clear());
}

/// The transcript keeps no timestamps for tool calls, so remember when the renderer first
/// saw one in flight and when it first saw it done. A call that is only ever seen finished
/// (a replayed session) has no duration and returns `None`.
pub fn took(id: &str, in_flight: bool, finished: bool) -> Option<Duration> {
    CLOCK.with(|c| {
        let mut c = c.borrow_mut();
        if in_flight {
            c.entry(id.to_string())
                .or_insert_with(|| (Instant::now(), None));
        }
        if finished {
            if let Some((start, end)) = c.get_mut(id) {
                return Some(*end.get_or_insert_with(|| start.elapsed()));
            }
        }
        None
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_call_clock_is_forgotten_on_reset() {
        assert_eq!(took("c1", true, false), None);
        assert!(took("c1", false, true).is_some());
        reset_clock();
        assert_eq!(took("c1", false, true), None);
    }

    #[test]
    fn wraps_words_and_breaks_after_slash_and_hyphen() {
        assert_eq!(wrap_text("aa bb cc", 5), vec!["aa bb", "cc"]);
        assert_eq!(wrap_text("aaaa/bbbb/cccc", 10), vec!["aaaa/bbbb/", "cccc"]);
        assert_eq!(
            wrap_text("more-words-more-words", 12),
            vec!["more-words-", "more-words"]
        );
        assert_eq!(wrap_text("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
        assert_eq!(wrap_text("a\n\nb", 9), vec!["a", "", "b"]);
        assert_eq!(wrap_text("    indented", 20), vec!["    indented"]);
    }

    #[test]
    fn hanging_spaces_do_not_start_a_row() {
        let line = "word ".repeat(80);
        let rows = wrap_text(line.trim_end(), 113);
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].matches("word").count(), 22);
        assert!(rows
            .iter()
            .all(|r| !r.starts_with(' ') && !r.ends_with(' ')));
    }

    #[test]
    fn paths_follow_opencodes_formatter() {
        assert_eq!(fmt_path("/a/b/c.rs", "/a/b"), "c.rs");
        assert_eq!(fmt_path("c.rs", "/a/b"), "c.rs");
        assert_eq!(fmt_path("/a/b", "/a/b"), ".");
        assert_eq!(fmt_path(".", "/a/b"), ".");
        assert_eq!(fmt_path("/a/bc/x", "/a/b"), "/a/bc/x");
        assert_eq!(fmt_path("../x", "/a/b"), "/a/x");
        assert_eq!(fmt_path("", "/a/b"), "");
    }

    #[test]
    fn collapse_matches_opencode() {
        let (s, o) = collapse("a\nb\nc", 2, 100);
        assert!(o);
        assert_eq!(s, "a\nb\n…");
        assert_eq!(collapse("a\nb", 2, 100), ("a\nb".to_string(), false));
        let (s, o) = collapse(&"x".repeat(50), 10, 20);
        assert!(o);
        assert_eq!(s.chars().count(), 20);
    }

    #[test]
    fn input_args_lists_primitives_only() {
        let v = serde_json::json!({"path": "a", "n": 3, "flag": true, "obj": {"x": 1}});
        assert_eq!(input_args(&v, &["path"]), "[n=3, flag=true]");
        assert_eq!(input_args(&serde_json::json!({}), &[]), "");
    }

    #[test]
    fn titlecase_like_locale() {
        assert_eq!(titlecase("general task"), "General Task");
        assert_eq!(titlecase("read_file"), "Read_file");
    }

    #[test]
    fn output_is_stripped_and_trimmed() {
        assert_eq!(clean_output("\x1b[31mred\x1b[0m plain\r\n\n"), "red plain");
        // openw draws a tab as two cells everywhere, as opencode does
        tuikit::width::set_tab_width(2);
        assert_eq!(clean_output("a\tb"), "a  b");
    }
}
