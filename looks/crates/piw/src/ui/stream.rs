// OWNER: stream (long text rendered in chunks that are not redone while it grows)
//! `markdown_theme::render` on the whole text of a message every frame costs O(message), so an
//! answer that streams for a while falls below 60 fps (65 ms a frame at 1 MB) and a flood is
//! drawn once per 64 KB of text, each time from the start.
//!
//! A long text is cut at paragraph boundaries into chunks of about [`CHUNK`] bytes. A chunk is
//! rendered once and kept; a frame renders the chunks it has not seen and the short tail after
//! the last cut. The text is append-only while it streams, so the cache is keyed by the text
//! itself (a prefix compare, a memcmp), never by a length.
//!
//! The cut is only made where it cannot change how the rest renders: after a blank line, outside
//! a fence, between two plain paragraph-like lines (no list, quote, table, indented, HTML, link
//! definition or setext lines on either side). Two blocks that sit apart in the source render
//! with one blank row between them, which is what the join adds, and a test pins that against
//! the whole-text render. A text with no such boundary is not cut at all until it passes
//! [`FORCE`], and then at a line end.

use std::cell::RefCell;

use ratatui::style::Style;

use super::markdown_theme;
use super::{blank, Lines};
use crate::theme::PiTheme;

/// Texts shorter than this are rendered whole.
pub const CHUNK: usize = 16 * 1024;
/// A tail with no safe boundary is cut at a line end once it is this long.
const FORCE: usize = 256 * 1024;
const ENTRIES: usize = 6;

struct Entry {
    key: (u16, String, String),
    /// The text the chunks cover.
    text: String,
    chunks: Vec<Lines>,
}

thread_local! {
    static CACHE: RefCell<Vec<Entry>> = const { RefCell::new(Vec::new()) };
}

/// Forget every kept chunk (a session switch).
pub fn clear() {
    CACHE.with(|c| c.borrow_mut().clear());
}

/// `markdown_theme::render`, with the finished chunks of a long text kept between calls.
pub fn render_md(src: &str, width: u16, theme: &PiTheme, base: Style) -> Lines {
    if src.len() < 2 * CHUNK {
        return markdown_theme::render(src, width, theme, base);
    }
    let key = (width, theme.name.clone(), format!("{base:?}"));
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        let at = match c
            .iter()
            .position(|e| e.key == key && src.starts_with(&e.text))
        {
            Some(i) => i,
            None => {
                if c.len() >= ENTRIES {
                    c.remove(0);
                }
                c.push(Entry {
                    key,
                    text: String::new(),
                    chunks: Vec::new(),
                });
                c.len() - 1
            }
        };
        // most recently used last
        let last = c.len() - 1;
        c.swap(at, last);
        let e = &mut c[last];
        // new chunks, while the text past the covered part holds another cut
        while let Some(cut) = next_cut(&src[e.text.len()..]) {
            let start = e.text.len();
            let piece = src[start..start + cut].trim();
            e.chunks
                .push(markdown_theme::render(piece, width, theme, base));
            e.text.push_str(&src[start..start + cut]);
        }
        let mut out: Lines = Vec::new();
        for ch in &e.chunks {
            out.extend(ch.iter().cloned());
            out.push(blank());
        }
        let tail = src[e.text.len()..].trim();
        if !tail.is_empty() {
            out.extend(markdown_theme::render(tail, width, theme, base));
        } else if !e.chunks.is_empty() {
            out.pop();
        }
        out
    })
}

/// Where to cut `s` (the text after what is already covered): the byte offset just past a blank
/// line, at least [`CHUNK`] bytes in, or `None`. The text before it is rendered as a chunk.
fn next_cut(s: &str) -> Option<usize> {
    if s.len() < CHUNK + 1024 {
        return None;
    }
    let mut in_fence: Option<(char, usize)> = None;
    let mut offset = 0;
    // (offset after the blank line, whether the line before it was plain)
    let mut pending: Option<(usize, bool)> = None;
    let mut prev_plain = false;
    let mut prev_blank = false;
    let mut forced: Option<usize> = None;
    for line in s.split_inclusive('\n') {
        let complete = line.ends_with('\n');
        let body = line.trim_end_matches(['\n', '\r']);
        let end = offset + line.len();
        if let Some((ch, n)) = in_fence {
            let t = body.trim_start();
            let run = t.chars().take_while(|c| *c == ch).count();
            if run >= n && t[run..].trim().is_empty() {
                in_fence = None;
                prev_plain = true;
                prev_blank = false;
            }
        } else if body.trim().is_empty() {
            if complete && !prev_blank && offset >= CHUNK / 2 {
                pending = Some((end, prev_plain));
            }
            prev_blank = true;
        } else {
            let plain = is_plain(body);
            if let Some((cut, before_plain)) = pending.take() {
                // a boundary counts once the line after it is known to be plain too
                if before_plain && plain && cut >= CHUNK {
                    return Some(cut);
                }
            }
            let t = body.trim_start();
            let fence = t.chars().next().filter(|c| matches!(c, '`' | '~'));
            match fence {
                Some(ch) if body.len() == t.len() || body.len() - t.len() < 4 => {
                    let n = t.chars().take_while(|c| *c == ch).count();
                    if n >= 3 {
                        in_fence = Some((ch, n));
                    }
                }
                _ => {}
            }
            prev_plain = plain && in_fence.is_none();
            prev_blank = false;
            if in_fence.is_none() && complete && forced.is_none() && end >= FORCE {
                forced = Some(end);
            }
        }
        if in_fence.is_some() {
            prev_plain = false;
        }
        offset = end;
    }
    forced.filter(|f| *f < s.len())
}

/// A line that starts a paragraph or heading, or closes a fence, and cannot be part of a list,
/// quote, table, HTML block, link definition, indented code or setext underline.
fn is_plain(line: &str) -> bool {
    let Some(c) = line.chars().next() else {
        return false;
    };
    if c.is_whitespace() {
        return false;
    }
    if c == '#' || c == '`' || c == '~' {
        // a fence or a heading; `~~strike~~` text is not worth the risk
        return c != '~' || line.starts_with("~~~");
    }
    if c.is_ascii_digit() {
        let digits = line.chars().take_while(char::is_ascii_digit).count();
        let rest = &line[digits..];
        return !(rest.starts_with(['.', ')'])
            && rest[1..].chars().next().is_none_or(char::is_whitespace));
    }
    if c.is_alphabetic() {
        // a link definition is `[x]: url`, which starts with `[`; HTML starts with `<`
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn th() -> PiTheme {
        PiTheme::dark()
    }

    fn plain(l: &Lines) -> Vec<String> {
        l.iter()
            .map(|r| {
                r.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    fn doc(paras: usize) -> String {
        let mut s = String::new();
        for i in 0..paras {
            match i % 6 {
                0 => s.push_str(&format!("## Section {i}\n\n")),
                1 => s.push_str(&format!(
                    "Paragraph {i} with **bold**, `code`, a [link](http://x/{i}) and enough words to wrap around the edge of a narrow terminal at least once or twice.\n\n"
                )),
                2 => s.push_str(&format!("```rust\nfn f{i}() {{\n    let x = {i};\n\n    x + 1\n}}\n```\n\n")),
                3 => s.push_str(&format!("- item {i}\n- item two\n\n")),
                4 => s.push_str(&format!("| a | b |\n|---|---|\n| {i} | y |\n\n")),
                _ => s.push_str(&format!("{i}. numbered\n\nplain after list {i}\n\n")),
            }
        }
        s
    }

    #[test]
    fn a_chunked_render_is_the_whole_render() {
        let t = th();
        for paras in [30, 80, 2500] {
            let src = doc(paras);
            assert!(src.len() > 2 * CHUNK || paras < 100, "{}", src.len());
            for w in [40u16, 80, 120] {
                clear();
                let whole = markdown_theme::render(src.trim(), w, &t, Style::default());
                let chunked = render_md(src.trim(), w, &t, Style::default());
                assert_eq!(plain(&whole), plain(&chunked), "{paras} paragraphs at {w}");
                assert_eq!(whole, chunked, "styles differ, {paras} paragraphs at {w}");
                if paras > 100 {
                    CACHE.with(|c| assert!(c.borrow()[0].chunks.len() > 2));
                }
            }
        }
    }

    #[test]
    fn plain_paragraphs_are_cut_and_the_rest_is_reused() {
        let t = th();
        clear();
        let src: String = (0..2000)
            .map(|i| format!("Paragraph number {i} of plain words.\n\n"))
            .collect();
        assert!(src.len() > 4 * CHUNK);
        let a = render_md(src.trim(), 80, &t, Style::default());
        CACHE.with(|c| assert!(c.borrow()[0].chunks.len() >= 3));
        // the same text again, and a longer one, give the whole render's rows
        assert_eq!(a, render_md(src.trim(), 80, &t, Style::default()));
        let longer = format!("{src}and a last paragraph\n");
        let whole = markdown_theme::render(longer.trim(), 80, &t, Style::default());
        assert_eq!(whole, render_md(longer.trim(), 80, &t, Style::default()));
    }

    #[test]
    fn a_cut_never_lands_inside_a_fence_or_a_list() {
        let fence = format!(
            "intro\n\n```\n{}\n```\n\nafter\n",
            "line\n\nline\n".repeat(4000)
        );
        let c = next_cut(&fence);
        assert!(
            c.is_none_or(|c| fence[..c].matches("```").count() % 2 == 0),
            "{c:?}"
        );
        let list: String = (0..3000).map(|i| format!("{i}. item\n\n")).collect();
        assert_eq!(next_cut(&list), None, "numbered items are one list");
        let quote: String = (0..3000).map(|_| "> quoted\n\n".to_string()).collect();
        assert_eq!(next_cut(&quote), None);
    }

    #[test]
    fn a_text_edited_in_place_is_not_served_from_the_cache() {
        let t = th();
        clear();
        let a: String = (0..2000).map(|i| format!("alpha {i}\n\n")).collect();
        let b: String = (0..2000).map(|i| format!("bravo {i}\n\n")).collect();
        let ra = render_md(a.trim(), 80, &t, Style::default());
        let rb = render_md(b.trim(), 80, &t, Style::default());
        assert!(plain(&ra)[0].contains("alpha 0") && plain(&rb)[0].contains("bravo 0"));
    }
}
