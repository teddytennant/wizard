//! Agent markdown → the small HTML subset Telegram renders.
//!
//! # Why convert at all
//!
//! The agent answers in markdown, because that is what it answers in
//! everywhere. Sent verbatim, a Telegram client shows the punctuation: literal
//! `**bold**`, and code blocks as a wall of unindented prose with their fences
//! still in them. The one thing a chat reply most needs to get right — a
//! command or a patch the reader is going to copy — is the thing that reads
//! worst.
//!
//! Telegram offers MarkdownV2 and HTML. HTML is the safer target by some
//! distance: MarkdownV2 requires escaping eighteen characters *including
//! inside* pre-formatted blocks, and one missed backslash is a rejected
//! message rather than a cosmetic slip. HTML needs three (`&`, `<`, `>`), the
//! rule is uniform, and the tag set is tiny.
//!
//! # The failure mode this is built around
//!
//! A malformed conversion is not a formatting bug, it is a *lost reply*:
//! `sendMessage` answers 400 and the text never arrives. So the transport
//! sends HTML and falls back to plain text on rejection, and everything here
//! is written to make that fallback rare rather than to be clever. Unsupported
//! constructs degrade to plain text rather than to a guess: Telegram has no
//! headings, so a heading becomes bold; it has no list markup, so a bullet
//! stays the character the model typed.

/// Telegram's HTML entities, applied to every span of text that is not a tag
/// this module emitted. `&` first, or it would double-escape the others.
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// A fenced code block's info string reduced to a bare language token.
///
/// ` ```rust,no_run ` and ` ```rust title="x" ` both mean rust. Telegram wants
/// `class="language-rust"` and ignores anything it does not know, so the
/// conservative read is the first word, alphanumerics only.
fn language_of(info: &str) -> Option<String> {
    let token: String = info
        .trim()
        .chars()
        .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '+' || *ch == '-' || *ch == '_')
        .collect();
    (!token.is_empty()).then(|| token.to_ascii_lowercase())
}

/// Convert one chunk of agent markdown to Telegram HTML.
///
/// Chunk, not document: the caller splits first (see [`super::split_message`]),
/// and that split is *not* fence-aware — it budgets UTF-16 code units and
/// breaks on line boundaries, knowing nothing about markdown. So a fenced block
/// can straddle two chunks and this function will see a fence that opens and
/// never closes.
///
/// That is deliberately survivable rather than prevented. The rule that makes
/// it survivable is one line down in the fence branch: the closing `</pre>` is
/// emitted whether or not a closing fence was found, so the output is balanced
/// markup either way. A chunk that opened a `<pre>` it never closed would be a
/// 400 from `sendMessage` — a *lost reply*, which is the failure this whole
/// module is arranged around — whereas the worst that a split fence costs today
/// is that the second chunk's code renders as prose until its stray fence
/// reopens the block. Formatting, not delivery.
pub fn to_telegram_html(markdown: &str) -> String {
    let mut out = String::with_capacity(markdown.len() + 32);
    let mut lines = markdown.lines().peekable();

    while let Some(line) = lines.next() {
        let trimmed = line.trim_start();
        // Fenced block: everything to the closing fence is verbatim.
        if let Some(info) = trimmed.strip_prefix("```") {
            let language = language_of(info);
            match &language {
                Some(lang) => out.push_str(&format!("<pre><code class=\"language-{lang}\">")),
                None => out.push_str("<pre>"),
            }
            let mut body = String::new();
            for line in lines.by_ref() {
                if line.trim_start().starts_with("```") {
                    break;
                }
                body.push_str(line);
                body.push('\n');
            }
            // Telegram renders the block's trailing newline as a blank line.
            out.push_str(escape(body.trim_end_matches('\n')).as_str());
            out.push_str(match language.is_some() {
                true => "</code></pre>\n",
                false => "</pre>\n",
            });
            continue;
        }

        // A heading has no Telegram equivalent; bold is the honest reduction.
        let (line, heading) = match trimmed.strip_prefix('#') {
            Some(rest) => {
                let text = rest.trim_start_matches('#').trim_start();
                (text, true)
            }
            None => (line, false),
        };

        if heading {
            out.push_str("<b>");
            out.push_str(&inline(line));
            out.push_str("</b>\n");
        } else {
            out.push_str(&inline(line));
            out.push('\n');
        }
    }
    // `lines()` drops a trailing newline; restore it only if there was one, so
    // a reply does not grow a blank line every time it passes through here.
    if !markdown.ends_with('\n') {
        let _ = out.pop();
    }
    out
}

/// Inline spans within one line: code first, because its contents must not be
/// re-read for emphasis. Anything unmatched is left as the literal text it was.
fn inline(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        // `code`
        if chars[i] == '`'
            && let Some(end) = find_from(&chars, i + 1, '`')
        {
            let body: String = chars[i + 1..end].iter().collect();
            out.push_str("<code>");
            out.push_str(&escape(&body));
            out.push_str("</code>");
            i = end + 1;
            continue;
        }
        // **bold** — checked before `*italic*` so the longer marker wins.
        if let Some((body, next)) = paired(&chars, i, "**") {
            out.push_str("<b>");
            out.push_str(&inline(&body));
            out.push_str("</b>");
            i = next;
            continue;
        }
        if word_internal(&chars, i, 2) {
            // `snake__case`: underscores inside a word are a name, not
            // emphasis. Markdown itself makes this exception and so must we,
            // or every identifier in a reply grows italics.
            out.push_str(&escape(&chars[i].to_string()));
            i += 1;
            continue;
        }
        if let Some((body, next)) = paired(&chars, i, "__") {
            out.push_str("<b>");
            out.push_str(&inline(&body));
            out.push_str("</b>");
            i = next;
            continue;
        }
        // *italic* / _italic_
        if let Some((body, next)) = paired(&chars, i, "*") {
            out.push_str("<i>");
            out.push_str(&inline(&body));
            out.push_str("</i>");
            i = next;
            continue;
        }
        if word_internal(&chars, i, 1) {
            out.push_str(&escape(&chars[i].to_string()));
            i += 1;
            continue;
        }
        if let Some((body, next)) = paired(&chars, i, "_") {
            out.push_str("<i>");
            out.push_str(&inline(&body));
            out.push_str("</i>");
            i = next;
            continue;
        }
        out.push_str(&escape(&chars[i].to_string()));
        i += 1;
    }
    out
}

/// Whether an underscore run at `i` sits inside a word.
///
/// `snake_case_name` is an identifier, and treating its underscores as
/// emphasis would italicise the middle of every symbol the agent mentions.
/// Markdown draws the same exception; the test that caught this was written
/// before the code that needed it.
fn word_internal(chars: &[char], i: usize, len: usize) -> bool {
    if chars.get(i) != Some(&'_') {
        return false;
    }
    let before = i.checked_sub(1).and_then(|at| chars.get(at));
    let after = chars.get(i + len);
    before.is_some_and(|ch| ch.is_alphanumeric()) && after.is_some_and(|ch| ch.is_alphanumeric())
}

/// The next index at or after `from` holding `needle`.
fn find_from(chars: &[char], from: usize, needle: char) -> Option<usize> {
    chars[from..]
        .iter()
        .position(|ch| *ch == needle)
        .map(|at| at + from)
}

/// A span delimited by `marker` starting at `i`, and the index just past it.
///
/// `None` when the marker does not open here, when it never closes on this
/// line, or when the span is empty — an unmatched `*` is far more often
/// multiplication or a bullet than it is emphasis, and turning it into a tag
/// would be the conversion inventing formatting nobody wrote.
fn paired(chars: &[char], i: usize, marker: &str) -> Option<(String, usize)> {
    let marker: Vec<char> = marker.chars().collect();
    if !chars[i..].starts_with(&marker[..]) {
        return None;
    }
    let body_start = i + marker.len();
    let mut at = body_start;
    while at + marker.len() <= chars.len() {
        if chars[at..].starts_with(&marker[..]) {
            if at == body_start {
                return None;
            }
            return Some((chars[body_start..at].iter().collect(), at + marker.len()));
        }
        at += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fenced_block_becomes_pre_code_with_its_language() {
        let html = to_telegram_html("before\n```rust\nlet x = 1;\n```\nafter");
        assert!(
            html.contains("<pre><code class=\"language-rust\">let x = 1;</code></pre>"),
            "{html}"
        );
        assert!(html.starts_with("before\n"), "{html}");
        assert!(html.trim_end().ends_with("after"), "{html}");
    }

    #[test]
    fn a_fence_with_no_language_is_still_pre() {
        let html = to_telegram_html("```\nplain\n```");
        assert!(html.contains("<pre>plain</pre>"), "{html}");
    }

    #[test]
    fn code_contents_are_escaped_and_never_re_read_for_emphasis() {
        // The classic: a shell redirect and a glob inside code. If either the
        // escaping or the ordering were wrong this would emit a stray tag and
        // Telegram would reject the whole message.
        let html = to_telegram_html("```sh\ngrep -r \"a<b\" . > out.txt && echo *done*\n```");
        assert!(html.contains("a&lt;b"), "{html}");
        assert!(html.contains("&gt; out.txt"), "{html}");
        assert!(
            html.contains("*done*"),
            "emphasis inside a code block must stay literal: {html}"
        );
        assert!(!html.contains("<i>"), "{html}");
    }

    #[test]
    fn inline_code_wins_over_emphasis() {
        let html = to_telegram_html("use `a * b` not *a*");
        assert!(html.contains("<code>a * b</code>"), "{html}");
        assert!(html.contains("<i>a</i>"), "{html}");
    }

    #[test]
    fn bold_beats_italic_and_nests() {
        assert!(to_telegram_html("**x**").contains("<b>x</b>"));
        assert!(to_telegram_html("*x*").contains("<i>x</i>"));
        assert!(to_telegram_html("**a `b` c**").contains("<code>b</code>"));
    }

    #[test]
    fn an_unmatched_marker_is_left_alone() {
        // 3 * 4 is arithmetic, and a bullet is a bullet. Inventing emphasis
        // here would be worse than doing nothing.
        for text in ["3 * 4 = 12", "* a bullet", "snake_case_name", "a ** b"] {
            let html = to_telegram_html(text);
            assert!(!html.contains("<i>"), "{text:?} -> {html}");
            assert!(!html.contains("<b>"), "{text:?} -> {html}");
        }
    }

    #[test]
    fn html_in_prose_is_escaped() {
        let html = to_telegram_html("compare <script> & <b>tags</b>");
        assert!(!html.contains("<script>"), "{html}");
        assert!(html.contains("&lt;script&gt;"), "{html}");
        assert!(html.contains("&amp;"), "{html}");
        // The escaping must not double-apply.
        assert!(!html.contains("&amp;lt;"), "{html}");
    }

    #[test]
    fn a_heading_becomes_bold_because_telegram_has_none() {
        let html = to_telegram_html("## Suggested order\ntext");
        assert!(html.contains("<b>Suggested order</b>"), "{html}");
    }

    /// The claim [`to_telegram_html`]'s own doc rests on, pinned.
    ///
    /// The caller's split is not fence-aware, so a long answer with code in it
    /// routinely hands this function a chunk whose fence opens and never
    /// closes, or one that begins with the closing half of somebody else's
    /// block. Unbalanced markup out of here is a 400 from `sendMessage`, and a
    /// 400 is not a formatting slip — it is a reply that never arrives, which
    /// from the chat is the agent having said nothing at all.
    #[test]
    fn a_chunk_cut_through_a_code_fence_still_produces_balanced_markup() {
        for chunk in [
            "here it is:\n```rust\nlet x = 1;",
            "let y = 2;\n```\nand that is the end",
            "```",
            "```sh\n",
            "text with a stray ` backtick and *asterisk",
        ] {
            let html = to_telegram_html(chunk);
            // `<pre` rather than `<pre>`, so the `<pre><code class=…>` form
            // counts once; `</pre>` contains neither, so the two never
            // double-count each other.
            assert_eq!(
                html.matches("<pre").count(),
                html.matches("</pre>").count(),
                "{chunk:?} -> {html}"
            );
            assert_eq!(
                html.matches("<code").count(),
                html.matches("</code>").count(),
                "{chunk:?} -> {html}"
            );
            for (open, close) in [("<b>", "</b>"), ("<i>", "</i>")] {
                assert_eq!(
                    html.matches(open).count(),
                    html.matches(close).count(),
                    "{chunk:?} -> {html}"
                );
            }
        }
    }

    #[test]
    fn plain_text_survives_unchanged_including_its_trailing_newline() {
        assert_eq!(to_telegram_html("hello"), "hello");
        assert_eq!(to_telegram_html("hello\n"), "hello\n");
        assert_eq!(to_telegram_html("a\nb"), "a\nb");
    }
}
