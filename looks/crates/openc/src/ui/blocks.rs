// OWNER: transcript
//! Layouts for the message blocks: welcome, user, thought, notice, error, interrupted and the
//! turn footer. Assistant prose is `md`, tool cards are `tools`.

use std::time::Duration;

use agent_core::NoticeLevel;
use ratatui::style::{Modifier, Style};
use ratatui::text::Span;
use tuikit::width::{display_width, spans_width, wrap_spans, WrapMode};

use super::md;
use super::row::{cut, fmt_dur, fmt_dur_long, fmt_tokens, sp, spaces, wrap_plain, Cx, Row};

#[derive(Clone, Debug, Default)]
pub struct WelcomeInfo {
    pub version: String,
    pub backend: String,
    pub model: String,
    pub effort: String,
    pub cwd: String,
    pub branch: String,
    /// Title and age of the previous session in this directory.
    pub last: Option<(String, String)>,
}

/// Rows in the welcome block. Fixed: the block is the first thing in the transcript and rows
/// that arrive later (the model after `Ready`, the last session once the list is read) must not
/// push what comes below it.
pub const WELCOME_ROWS: usize = 7;

/// The first screen: a letterhead, not a banner. A spaced lowercase wordmark in the accent with
/// a short accent rule under it, the model and backend under that, then what is worth knowing
/// before the first message: the keys that are not in the composer's placeholder, and the
/// session to go back to. The path is on the status row and is not repeated here.
///
/// ```text
///   o p e n c  0.1
///   ━━━━━━━━━──────────────────────
///   sonnet-5.5 high · claude 2.1.290
///
///   shift+tab mode   ctrl+t model   ctrl+p commands   f1 keys
///   /resume "Fix the flaky parser test" 14d ago
/// ```
///
/// A row that does not fit loses whole segments from the right, never half of one: a bare `…`
/// standing in for a key hint read as a bug (round 1 finding 10).
pub fn welcome(w: &WelcomeInfo, cx: &Cx) -> Vec<Row> {
    let p = cx.p;
    let g = cx.g;
    let width = cx.width;
    // Text starts on the prose column, two cells in, like everything else in the transcript.
    let indent = if width >= 20 { 2 } else { 0 };
    // And two cells of right margin: nothing is drawn in the last column.
    let room = width.saturating_sub(indent + 2);
    let row = |spans: Vec<Span<'static>>| {
        let mut v = vec![spaces(indent)];
        v.extend(spans);
        Row::new(v)
    };
    let fit = |tiers: Vec<Vec<Span<'static>>>| -> Vec<Span<'static>> {
        // Longest first; the last is the fallback and is cut to the room.
        tiers
            .iter()
            .find(|t| spans_width(t) <= room)
            .cloned()
            .unwrap_or_else(|| {
                let last = tiers.last().cloned().unwrap_or_default();
                tuikit::width::clip_spans(last, room)
            })
    };
    let gap = || spaces(3);
    let sep = || sp(format!(" {} ", g.sep), p.s_faint());

    // 1. The wordmark.
    let mut mark = vec![sp("o p e n c", p.s_accent().add_modifier(Modifier::BOLD))];
    if !w.version.is_empty() {
        mark.push(sp(format!("  {}", w.version), p.s_faint()));
    }
    // 2. An accent rule as long as the wordmark under a faint one, `━` and `─` like the gauge.
    let rule_w = room.min(44);
    let accent_w = 9.min(rule_w);
    let rule = vec![
        sp(g.gauge_on.repeat(accent_w), p.s_accent()),
        sp(g.gauge_off.repeat(rule_w - accent_w), p.s_line()),
    ];
    // 3. Who you are talking to: the model in the text colour, the rest quieter.
    let mut tiers = Vec::new();
    let model = |with_effort: bool, with_backend: bool| {
        let mut v = Vec::new();
        if !w.model.is_empty() {
            v.push(sp(w.model.clone(), p.s_text()));
            if with_effort && !w.effort.is_empty() {
                v.push(sp(format!(" {}", w.effort), p.s_dim()));
            }
        }
        if with_backend && !w.backend.is_empty() {
            if !v.is_empty() {
                v.push(sep());
            }
            v.push(sp(w.backend.clone(), p.s_dim()));
        }
        v
    };
    for (e, b) in [(true, true), (true, false), (false, false)] {
        tiers.push(model(e, b));
    }
    let who = fit(tiers);
    // 4. Hints, whole ones from the right dropped first.
    let hint = |key: &str, label: &str| {
        vec![
            sp(key.to_string(), p.s_dim()),
            sp(format!(" {label}"), p.s_faint()),
        ]
    };
    let all = [
        hint("shift+tab", "mode"),
        hint("ctrl+t", "model"),
        hint("ctrl+p", "commands"),
        hint("f1", "keys"),
    ];
    let join = |n: usize| -> Vec<Span<'static>> {
        let mut out = Vec::new();
        for (i, h) in all.iter().take(n).enumerate() {
            if i > 0 {
                out.push(gap());
            }
            out.extend(h.iter().cloned());
        }
        out
    };
    let hints = fit((1..=4).rev().map(join).collect());
    // 5. The session to go back to, its title cut to what is left.
    let last = w.last.as_ref().map(|(title, age)| {
        let tail = format!(" {age}");
        let head = spans_width(&[sp("/resume ", p.s_dim())]);
        let t_room = room.saturating_sub(head + 2 + display_width(&tail));
        let mut v = vec![sp("/resume ", p.s_dim())];
        if t_room >= 8 {
            v.push(sp(format!("\"{}\"", cut(title, t_room)), p.s_faint()));
            v.push(sp(tail, p.s_faint()));
        }
        v
    });
    vec![
        Row::new(Vec::new()),
        row(mark),
        row(rule),
        row(who),
        Row::new(Vec::new()),
        row(hints),
        row(last.unwrap_or_default()),
    ]
}

/// Chips (`[paste 42 lines]`, `[image 1280x720]`) get their own look inside a user message.
fn chip_spans(line: &str, cx: &Cx) -> Vec<Span<'static>> {
    let p = cx.p;
    let text = p.s_text();
    let chip = Style::new().fg(p.dim).bg(p.line);
    let mut out = Vec::new();
    let mut rest = line;
    while let Some(i) = rest.find('[') {
        let tail = &rest[i..];
        let is_chip =
            (tail.starts_with("[paste ") || tail.starts_with("[image ")) && tail.contains(']');
        if !is_chip {
            out.push(sp(&rest[..i + 1], text));
            rest = &rest[i + 1..];
            continue;
        }
        let end = tail.find(']').unwrap_or(0) + 1;
        if i > 0 {
            out.push(sp(&rest[..i], text));
        }
        out.push(sp(&tail[..end], chip));
        rest = &tail[end..];
    }
    if !rest.is_empty() {
        out.push(sp(rest, text));
    }
    out
}

pub fn user(shown: &str, cx: &Cx) -> Vec<Row> {
    let p = cx.p;
    let w = cx.width.saturating_sub(3).max(8);
    let mut rows = Vec::new();
    for line in tuikit::width::normalize_newlines(shown).split('\n') {
        let spans = chip_spans(line, cx);
        let wrapped = if spans.is_empty() {
            vec![Vec::new()]
        } else {
            wrap_spans(&spans, w, WrapMode::Word)
        };
        for l in wrapped {
            let mut r = vec![sp(cx.g.bar, p.s_user()), spaces(1)];
            r.extend(l);
            rows.push(Row::new(r).bg(p.raised));
        }
    }
    rows
}

/// Fewest words a sentence needs to be worth showing on the live row.
const MIN_SENTENCE_WORDS: usize = 4;

/// Front of a sentence trimmed to its first letter or digit, and a `Line 7:` label dropped:
/// a fragment from the middle of a quote or a list would show a dangling mark (finding 16:
/// `"Shell`, `Line 7: (`).
fn tidy_sentence(s: &str) -> &str {
    let s = s.trim().trim_start_matches(|c: char| !c.is_alphanumeric());
    // `Line 7: (` leaves a label and nothing worth showing; `Line 7: read the file` keeps
    // what comes after the label.
    let s = match s.strip_prefix("Line ") {
        Some(rest) => {
            let after = rest.trim_start_matches(|c: char| c.is_ascii_digit());
            match after.strip_prefix(':') {
                Some(r) if after.len() < rest.len() => {
                    r.trim_start_matches(|c: char| !c.is_alphanumeric())
                }
                _ => s,
            }
        }
        None => s,
    };
    s.trim()
}

/// The last finished sentence of a growing thought that has at least [`MIN_SENTENCE_WORDS`]
/// words, for the live one-liner. The sentence being written is never shown: its tail is a
/// fragment (`…en a flat list and a tree`, `Actually, "`, `13.`), and the row would change
/// with every word, which tells the reader nothing (round 2 finding 7). Nothing is shown until
/// there is something whole to say.
fn last_sentence(t: &str) -> Option<&str> {
    let t = t.trim();
    // Ends: a `.`, `!` or `?` followed by whitespace or the end of the text, or a newline.
    let mut ends: Vec<usize> = Vec::new();
    let mut chars = t.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let stop = match c {
            '\n' => true,
            '.' | '!' | '?' => chars.peek().is_none_or(|(_, n)| n.is_whitespace()),
            _ => false,
        };
        if stop {
            ends.push(i + c.len_utf8());
        }
    }
    let mut upto = ends.len();
    while upto > 0 {
        let end = ends[upto - 1];
        let from = if upto > 1 { ends[upto - 2] } else { 0 };
        let s = tidy_sentence(&t[from..end]);
        let s = s.trim_end_matches(['.', '!', '?']).trim_end();
        if s.split_whitespace().count() >= MIN_SENTENCE_WORDS {
            return Some(s);
        }
        upto -= 1;
    }
    None
}

/// `s` cut from the end to `max` cells at a word boundary, with `ellipsis`; a single word wider
/// than that is cut like any other string.
fn cut_at_word(s: &str, max: usize, ellipsis: &str) -> String {
    if display_width(s) <= max {
        return s.to_string();
    }
    let room = max.saturating_sub(display_width(ellipsis));
    let mut out = String::new();
    for word in s.split(' ') {
        let next = if out.is_empty() {
            word.to_string()
        } else {
            format!("{out} {word}")
        };
        if display_width(&next) > room {
            break;
        }
        out = next;
    }
    if out.is_empty() {
        return tuikit::width::truncate(s, max);
    }
    format!("{out}{ellipsis}")
}

pub fn thought(
    text: &str,
    took: Option<Duration>,
    replayed: bool,
    open: bool,
    cx: &Cx,
    elapsed: Duration,
) -> Vec<Row> {
    let p = cx.p;
    let g = cx.g;
    let mut rows = Vec::new();
    match took {
        None => {
            let mut spans = vec![
                spaces(4),
                sp(
                    format!("{} thinking {} {}", g.think, g.sep, fmt_dur(elapsed)),
                    p.s_faint(),
                ),
            ];
            let used: usize = spans
                .iter()
                .map(|s| tuikit::width::display_width(&s.content))
                .sum();
            let room = cx.width.saturating_sub(used + 3);
            if let Some(last) = last_sentence(text).filter(|_| room > 8) {
                let one_line: String = last.split_whitespace().collect::<Vec<_>>().join(" ");
                let shown = cut_at_word(&one_line, room, cx.g.ellipsis);
                spans.push(sp(format!("  {shown}"), p.s_faint()));
            }
            rows.push(Row::new(spans).head());
        }
        Some(d) => {
            let label = if replayed || d.is_zero() {
                format!("{} thought", g.think)
            } else {
                format!("{} thought {}", g.think, fmt_dur(d))
            };
            let glyph = if open { g.unfolded } else { g.folded };
            rows.push(
                Row::new(vec![
                    spaces(2),
                    sp(glyph, p.s_dim()),
                    spaces(1),
                    sp(label, p.s_faint()),
                ])
                .head(),
            );
        }
    }
    if open && took.is_some() {
        let w = cx.width.saturating_sub(5).max(8);
        for l in text.lines() {
            for piece in wrap_plain(l, w) {
                rows.push(Row::new(vec![spaces(4), sp(piece, p.s_dim())]));
            }
        }
    }
    rows
}

pub fn notice(level: NoticeLevel, text: &str, cx: &Cx) -> Vec<Row> {
    let p = cx.p;
    let multi = text.trim().contains('\n');
    match level {
        NoticeLevel::Error => error(text, cx),
        _ if multi => {
            // Command output: markdown, quiet.
            let mut rows = md::render(text, cx);
            for r in &mut rows {
                for s in &mut r.spans {
                    if s.style.fg == Some(p.text) && s.style.bg.is_none() {
                        s.style = s.style.fg(p.dim);
                    }
                }
            }
            rows
        }
        lvl => {
            let st = if lvl == NoticeLevel::Warn {
                p.s_warn()
            } else {
                p.s_faint()
            };
            // One message, wrapped under its marker when it is longer than the column.
            let w = cx.width.saturating_sub(4).max(8);
            wrap_plain(text.trim(), w)
                .into_iter()
                .enumerate()
                .map(|(i, l)| {
                    let lead = if i == 0 {
                        format!("{} ", cx.g.sep)
                    } else {
                        "  ".to_string()
                    };
                    Row::new(vec![spaces(2), sp(format!("{lead}{l}"), st)])
                })
                .collect()
        }
    }
}

/// `✗ message` plus one faint row of detail; the rest folds behind `▸ details`.
pub fn error(text: &str, cx: &Cx) -> Vec<Row> {
    let p = cx.p;
    let mut lines = text.trim().lines();
    let first = lines.next().unwrap_or("error");
    let detail: Vec<&str> = lines.filter(|l| !l.trim().is_empty()).collect();
    let room = cx.width.saturating_sub(5);
    let mut rows = vec![Row::new(vec![
        spaces(2),
        sp(cx.g.fail, p.s_err()),
        spaces(1),
        sp(cut(first, room), p.s_text()),
    ])];
    if let Some(d) = detail.first() {
        rows.push(Row::new(vec![
            spaces(4),
            sp(cut(d.trim(), room), p.s_faint()),
        ]));
    }
    rows
}

/// A request that failed and is being tried again: the error row, then one faint row with the
/// attempt and the countdown, which ticks once a second while it is the open row.
pub fn retry(
    reason: &str,
    attempt: u32,
    max: u32,
    until: std::time::Instant,
    settled: bool,
    cx: &Cx,
) -> Vec<Row> {
    let detail = if settled {
        format!("retried {attempt} of {max}")
    } else {
        let secs = until.saturating_duration_since(cx.now).as_secs_f64().ceil() as u64;
        if secs == 0 {
            format!("retry {attempt} of {max} now {} ctrl+c cancels", cx.g.sep)
        } else {
            format!(
                "retry {attempt} of {max} in {secs}s {} ctrl+c cancels",
                cx.g.sep
            )
        }
    };
    error(
        &format!("Request failed {} {reason}\n{detail}", cx.g.sep),
        cx,
    )
}

pub fn fatal(text: &str, cx: &Cx) -> Vec<Row> {
    let p = cx.p;
    let mut rows = error(text, cx);
    rows.push(Row::new(vec![
        spaces(4),
        sp(
            "send a message to restart  your draft and queue are kept",
            p.s_faint(),
        ),
    ]));
    rows
}

pub fn interrupted(took: Duration, cx: &Cx) -> Vec<Row> {
    let p = cx.p;
    vec![Row::new(vec![
        spaces(2),
        sp(
            format!("{} interrupted {} {}", cx.g.stop, cx.g.sep, fmt_dur(took)),
            p.s_warn(),
        ),
    ])]
}

pub fn footer(took: Duration, out_tokens: u64, cost: Option<f64>, cx: &Cx) -> Vec<Row> {
    let p = cx.p;
    let mut parts = vec![fmt_dur_long(took)];
    if out_tokens > 0 {
        parts.push(format!("{} out", fmt_tokens(out_tokens)));
    }
    if let Some(c) = cost.filter(|c| *c >= 0.005) {
        parts.push(format!("${c:.2}"));
    }
    vec![Row::new(vec![
        spaces(2),
        sp(cx.g.ok, p.s_ok()),
        sp(
            format!(" {}", parts.join(&format!(" {} ", cx.g.sep))),
            p.s_faint(),
        ),
    ])]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::palette::{Depth, Kind, Palette, UNICODE};
    use std::time::Instant;

    fn info() -> WelcomeInfo {
        WelcomeInfo {
            version: "0.1".into(),
            backend: "claude 2.1.289".into(),
            model: "sonnet-5.5".into(),
            effort: "high".into(),
            cwd: "/tmp/visr1-work/proj".into(),
            branch: "main*".into(),
            last: Some((
                "Reply with exactly five words about terminals".into(),
                "just now".into(),
            )),
        }
    }

    #[test]
    fn welcome_never_cuts_a_hint_or_ends_a_row_on_an_ellipsis() {
        // Round 1 findings 10 and 28 (`ctrl+…`, `default…`), and the round 2 letterhead.
        for width in [30, 36, 44, 60, 76, 100, 160] {
            let rows = run(width, |cx| welcome(&info(), cx));
            assert_eq!(rows.len(), WELCOME_ROWS, "the block height never changes");
            assert!(rows[0].text().trim().is_empty());
            for r in &rows {
                let t = r.text();
                assert!(display_width(&t) <= width, "{width}: {t:?}");
                assert!(!t.trim_end().ends_with('…'), "{width}: {t:?}");
                // A hint is whole or absent.
                for (head, whole) in [
                    ("shift", "shift+tab mode"),
                    ("ctrl+t", "ctrl+t model"),
                    ("ctrl+p", "ctrl+p commands"),
                    ("f1", "f1 keys"),
                ] {
                    if t.contains(head) {
                        assert!(t.contains(whole), "{width}: {t:?}");
                    }
                }
            }
            // The wordmark starts on the second row, in the accent, bold.
            assert!(
                rows[1].text().trim_start().starts_with("o p e n c  0.1"),
                "{:?}",
                rows[1].text()
            );
            let mark = &rows[1].spans[1];
            assert_eq!(
                mark.style.fg,
                Some(Palette::new(Kind::Hearth, Depth::True).accent)
            );
            assert!(mark.style.add_modifier.contains(Modifier::BOLD));
            // The path is the status row's, not the welcome's.
            assert!(rows.iter().all(|r| !r.text().contains("/tmp/")), "{width}");
            // The model sits under the rule.
            assert!(
                rows[3].text().contains("sonnet-5.5"),
                "{width}: {:?}",
                rows[3].text()
            );
        }
        // Wide enough for everything, nothing is dropped.
        let rows = run(100, |cx| welcome(&info(), cx));
        assert!(
            rows[3].text().contains("sonnet-5.5 high") && rows[3].text().contains("claude 2.1.289")
        );
        assert!(
            [
                "shift+tab mode",
                "ctrl+t model",
                "ctrl+p commands",
                "f1 keys"
            ]
            .iter()
            .all(|h| rows[5].text().contains(h)),
            "{:?}",
            rows[5].text()
        );
        assert!(rows[6].text().contains("/resume") && rows[6].text().contains("just now"));
        // The rule is the wordmark's width in the accent, the rest in `line`.
        let rule = &rows[2];
        assert_eq!(
            rule.spans[1].style.fg,
            Some(Palette::new(Kind::Hearth, Depth::True).accent)
        );
        assert_eq!(
            rule.spans[2].style.fg,
            Some(Palette::new(Kind::Hearth, Depth::True).line)
        );
        // Before the backend has said hello the block is the same height and nothing is half-drawn.
        let bare = run(60, |cx| welcome(&WelcomeInfo::default(), cx));
        assert_eq!(bare.len(), WELCOME_ROWS);
    }

    fn run<R>(width: usize, f: impl FnOnce(&Cx) -> R) -> R {
        let p = Palette::new(Kind::Hearth, Depth::True);
        let t = p.theme();
        f(&Cx {
            p: &p,
            theme: &t,
            g: &UNICODE,
            width,
            detail: false,
            now: Instant::now(),
            spin: 0,
        })
    }

    #[test]
    fn user_rows_carry_the_bar_and_raised_background() {
        let rows = run(40, |cx| {
            user("hello there, this is a long message that must wrap", cx)
        });
        assert!(rows.len() >= 2);
        for r in &rows {
            assert!(r.text().starts_with("▎ "));
            assert!(r.bg.is_some());
        }
    }

    #[test]
    fn thought_one_liner_and_fold() {
        let live = run(60, |cx| {
            thought(
                "First idea. The lookahead is off by one. Next",
                None,
                false,
                false,
                cx,
                Duration::from_secs(8),
            )
        });
        assert_eq!(live.len(), 1);
        assert!(
            live[0].text().contains("thinking · 8.0s") && live[0].text().contains("The lookahead"),
            "{}",
            live[0].text()
        );
        let done = run(60, |cx| {
            thought(
                "body",
                Some(Duration::from_secs(8)),
                false,
                false,
                cx,
                Duration::ZERO,
            )
        });
        assert_eq!(done[0].text(), "  ▸ ◇ thought 8.0s");
        let open = run(60, |cx| {
            thought(
                "body",
                Some(Duration::from_secs(8)),
                false,
                true,
                cx,
                Duration::ZERO,
            )
        });
        assert_eq!(open.len(), 2);
        assert!(open[1].text().starts_with("    body"));
    }

    #[test]
    fn the_live_sentence_has_no_dangling_quote_bullet_or_line_label() {
        // Finding 16, from the real session: `◇ thinking · 0.6s  "Shell` and `Line 7: (`.
        assert_eq!(
            last_sentence("Let me check. \"Shell commands run in a pty\" is quoted. And more"),
            Some("Shell commands run in a pty\" is quoted")
        );
        assert_eq!(
            last_sentence("Next. - read the file now. Then"),
            Some("read the file now")
        );
        assert_eq!(last_sentence("Plan. Line 7: ("), None);
        assert_eq!(
            last_sentence("Plan. Line 7: read the whole file."),
            Some("read the whole file")
        );
        assert_eq!(
            last_sentence("Line seven is odd now."),
            Some("Line seven is odd now")
        );
        let live = run(60, |cx| {
            thought(
                "Hm. \"Shell",
                None,
                false,
                false,
                cx,
                Duration::from_millis(600),
            )
        });
        assert!(!live[0].text().contains('"'), "{}", live[0].text());
    }

    #[test]
    fn the_live_row_shows_a_whole_sentence_or_nothing_never_a_fragment() {
        // Round 2 finding 7: `thinking · 2.6s  Actually, "` and `thinking · 15s  13.`.
        for fragment in [
            "Actually, \"",
            "First idea. Actually, \"",
            "A real sentence is here. 13.",
            "Hmm.",
        ] {
            let live = run(80, |cx| {
                thought(fragment, None, false, false, cx, Duration::from_secs(15))
            });
            let t = live[0].text();
            let tail = t.split("thinking").nth(1).unwrap_or_default();
            assert!(
                !tail.contains("Actually") && !tail.contains("13") && !tail.contains("Hmm"),
                "{fragment:?} -> {t:?}"
            );
        }
        // A sentence that is not finished is not shown; a finished one is, whole.
        let t =
            last_sentence("Between a flat list and a tree, then check against the plan").is_none();
        assert!(t);
        let live = run(80, |cx| {
            thought(
                "Compare a flat list with a tree. Then check the result agai",
                None,
                false,
                false,
                cx,
                Duration::from_secs(3),
            )
        });
        assert!(
            live[0].text().ends_with("Compare a flat list with a tree"),
            "{:?}",
            live[0].text()
        );
    }

    #[test]
    fn a_long_sentence_is_cut_at_a_word_from_the_end_not_from_the_front() {
        let src = "The lookahead that decides where a code row breaks is computed before the row is committed, so nothing moves later.";
        for width in [30usize, 44, 60, 80, 120] {
            let live = run(width, |cx| {
                thought(src, None, false, false, cx, Duration::from_secs(9))
            });
            let t = live[0].text();
            assert!(display_width(&t) <= width, "{width}: {t:?}");
            if let Some(shown) = t.split("  ").last().filter(|s| s.ends_with('…')) {
                let words = shown.trim_end_matches('…');
                assert!(
                    src.starts_with(words.trim_end()),
                    "{width}: starts mid-sentence: {t:?}"
                );
                // Ends at a word boundary.
                let rest = &src[words.len()..];
                assert!(rest.starts_with(' '), "{width}: cut inside a word: {t:?}");
            }
        }
    }

    #[test]
    fn error_is_one_row_plus_detail() {
        let rows = run(60, |cx| {
            error("Request failed · overloaded (529)\nretry 2 of 5 in 8s", cx)
        });
        assert_eq!(rows.len(), 2);
        assert!(rows[0].text().starts_with("  ✗ Request failed"));
    }

    #[test]
    fn footer_skips_tiny_costs() {
        let r = run(60, |cx| {
            footer(Duration::from_millis(12_400), 3100, Some(0.04), cx)
        });
        assert_eq!(r[0].text(), "  ✓ 12.4s · 3.1k out · $0.04");
    }
}
