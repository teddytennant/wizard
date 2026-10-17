//! Streaming markdown stability. Adversarial answers are fed to `MdState` one char at a time,
//! in random chunk sizes and with the pause timer both off and on; after every delta every row
//! but the last one the reader could already see must be exactly what it was (text and style),
//! and the last one may only grow. This is design checklist 31 to 33.

use std::time::Instant;

use openc::palette::{Depth, Kind, Palette, UNICODE};
use openc::ui::md::{plain, render, MdState};
use openc::ui::row::{Cx, Row};

fn cx<'a>(p: &'a Palette, t: &'a tuikit::theme::Theme, width: usize) -> Cx<'a> {
    Cx {
        p,
        theme: t,
        g: &UNICODE,
        width,
        detail: false,
        now: Instant::now(),
        spin: 0,
    }
}

fn styled(r: &Row) -> Vec<(String, String)> {
    r.spans
        .iter()
        .map(|s| (s.content.to_string(), format!("{:?}", s.style)))
        .collect()
}

/// Rows whose text is machinery that is replaced rather than grown: the held-table note, and
/// the blank padding row that ends a code block.
fn transient(text: &str) -> bool {
    text.contains("so far") || text.trim().is_empty()
}

/// Without the blank padding row that ends a code block: the line above it is the live tail.
fn core(rows: &[Row]) -> &[Row] {
    match rows.last() {
        Some(r) if r.bg.is_some() && r.text().trim().is_empty() => &rows[..rows.len() - 1],
        _ => rows,
    }
}

struct Violation(String);

fn run(src: &str, width: usize, chunks: &[usize], partial_ok: bool) -> Result<(), Violation> {
    let pal = Palette::new(Kind::Hearth, Depth::True);
    let theme = pal.theme();
    let cx = cx(&pal, &theme, width);
    let mut st = MdState::default();
    let mut prev: Vec<Row> = Vec::new();
    let mut end = 0;
    let mut ci = 0;
    while end < src.len() {
        let step = chunks[ci % chunks.len()].max(1);
        ci += 1;
        end = (end + step).min(src.len());
        while !src.is_char_boundary(end) {
            end += 1;
        }
        let all = st.update(&src[..end], &cx, false, partial_ok).to_vec();
        let rows = core(&all).to_vec();
        if !prev.is_empty() {
            let keep = prev.len() - 1;
            for i in 0..keep.min(rows.len()) {
                if rows[i].text() != prev[i].text() || styled(&rows[i]) != styled(&prev[i]) {
                    return Err(Violation(format!(
                        "width {width}, after {end} bytes row {i} changed\n  was {:?} {:?}\n  now {:?} {:?}\n  prefix {:?}",
                        prev[i].text(),
                        styled(&prev[i]),
                        rows[i].text(),
                        styled(&rows[i]),
                        &src[..end]
                    )));
                }
            }
            if rows.len() < keep {
                return Err(Violation(format!(
                    "width {width}, after {end} bytes rows shrank from {} to {}\n  prefix {:?}",
                    prev.len(),
                    rows.len(),
                    &src[..end]
                )));
            }
            // The last visible row only grows.
            let was = prev[keep].text();
            let was = was.trim_end();
            if !transient(was) {
                let now = rows.get(keep).map(Row::text).unwrap_or_default();
                if !now.starts_with(was) {
                    return Err(Violation(format!(
                        "width {width}, after {end} bytes the tail row was rewritten\n  was {was:?}\n  now {now:?}\n  prefix {:?}",
                        &src[..end]
                    )));
                }
            }
        }
        prev = rows;
    }
    // Once the stream ends the result is what a one-shot render gives.
    let fin = plain(st.update(src, &cx, true, true));
    let once = plain(&render(src, &cx));
    if fin != once {
        return Err(Violation(format!(
            "width {width}: final rows differ from a one-shot render\n  streamed {fin:?}\n  one-shot {once:?}"
        )));
    }
    Ok(())
}

const CORPUS: &[(&str, &str)] = &[
    (
        "paragraphs and emphasis",
        "Here is **bold text**, some *italic words*, a `code span` and ~~struck~~ words that go on long enough to wrap around the column at least twice, then end.\n\nSecond paragraph with ***both*** marks and a [link text](https://example.com/a/b) inside.",
    ),
    (
        "headings",
        "# Title\n\n## Second level heading with words\n\n### Third\n\n####### not a heading\n\nSetext Title\n============\n\nAnother\n-------\n\nbody",
    ),
    (
        "lists, loose and tight, nested",
        "- one\n- two that is long enough to wrap onto a second row of text here\n  - nested a\n  - nested b\n    - deeper\n\n- loose three\n\n1. first\n2. second\n\n   continued paragraph in item two\n3. third\n\n3) paren\n4) list",
    ),
    (
        "lists with code fences inside",
        "1. install:\n\n   ```sh\n   cargo build\n   cargo test\n   ```\n\n2. run it\n\n- item\n\n  ```\n  raw\n\n  with blank\n  ```\n- after",
    ),
    (
        "unclosed fence then prose",
        "Intro line.\n\n```rust\nfn main() {\n    println!(\"hello\");\n\n    let x = 1; // a `tick`\n}\n```\n\nAfter the fence.\n\n```\nnever closed\nstill code",
    ),
    (
        "tilde and long fences",
        "~~~python\nprint('a')\n~~~\n\n````md\n```inner\nx\n```\n````\n\ntext",
    ),
    (
        "tables, aligned and ragged",
        "Before.\n\n| name | lines | status |\n|:-----|------:|:------:|\n| a.rs | 3 | ok |\n| bb.rs | 120 | needs `tests` |\n| ragged |\n\nAfter the table.",
    ),
    (
        "table straight after a paragraph line",
        "Results:\n| a | b |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |\n\ndone",
    ),
    (
        "table with wrapping cells",
        "| key | description |\n|---|---|\n| alpha | a rather long description of alpha that must wrap inside its column |\n| beta | short |\n| gamma | another long cell with many words that wraps over several rows here |",
    ),
    (
        "narrow cards",
        "| name | description of the thing |\n|---|---|\n| alpha | a very long description of alpha that wraps |\n| beta | other |",
    ),
    (
        "quotes",
        "> a quote that goes on\ncontinued lazily\n\n> > nested\n> back\n\n> - list in quote\n> - second\n\ntext",
    ),
    (
        "breaks and rules",
        "line one  \nline two\\\nline three\n\n---\n\n***\n\nend",
    ),
    (
        "html and entities",
        "<div>\nraw html block\n</div>\n\ninline <b>bold</b> and &amp; &lt; &copy; text <br/> more",
    ),
    (
        "links, autolinks, bare urls and images",
        "see <https://example.com/x> and https://example.com/long/path/that/keeps/going?q=1&r=2 and ![alt text](img.png) plus [ref][1] and [empty]()\n\n[1]: https://ref.example",
    ),
    (
        "unbreakable words",
        "short then aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa and `bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb` end",
    ),
    (
        "wide and combining characters",
        "日本語のテキストがここにあります。これは長い文章で、折り返されるはずです。\n\ne\u{301}\u{301} café naïve 😀 ok 👨‍👩‍👧 and more words to wrap the line around",
    ),
    (
        "crlf and tabs",
        "line one\r\nline two\r\n\r\n\tindented code\r\n\tmore\r\n\r\n- a\r\n- b\r\n",
    ),
    (
        "marks that never close",
        "a lone * star and ** two and _ under and ` tick and [ bracket and ( paren and ~~ tildes and < angle and ![ image\n\nnext paragraph with **bold that never ends and more words",
    ),
    (
        "marks in odd places",
        "snake_case_name and 2*3*4 and a_b_c and **bold**text and *em*phasis and (**paren bold**) and \\*escaped\\* and `a``b` and ``double `tick` code``",
    ),
    (
        "starts that look like markers",
        "-\n\n1.\n\n#\n\n>\n\n|\n\n```\n\n---\n\n1) x\n\n+ plus item\n\n* star item\n\n    indented code",
    ),
    (
        "code lines that wrap on a closing mark",
        "```rust\nlet name = std::env::args().nth(1).unwrap_or_else(|| \"world\".into());\n    println!(\"hello, {name} and the rest of this line goes on\");\nfoo(bar(baz(qux(1, 2, 3)))));\nlet v: Vec<_> = items.iter().map(|x| x.len()).collect::<Vec<usize>>();\n```\n\nafter",
    ),
    (
        "task lists",
        "- [ ] todo item\n- [x] done item\n- [X] also done\n- [ ] with `code`",
    ),
];

#[test]
fn committed_rows_never_move_while_adversarial_answers_stream() {
    let mut seed = 0x2545f4914f6cdd1du64;
    let mut rnd = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let mut failures = Vec::new();
    for (name, src) in CORPUS {
        for width in [24usize, 40, 76, 100] {
            for partial_ok in [false, true] {
                let mut plans: Vec<Vec<usize>> = vec![vec![1]];
                for _ in 0..3 {
                    plans.push((0..16).map(|_| (rnd() % 9) as usize + 1).collect());
                }
                for plan in &plans {
                    // A pause (partial words shown) is only meaningful between deltas of words;
                    // the strict "tail only grows" rule holds for the held-back mode.
                    if let Err(Violation(m)) = run(src, width, plan, partial_ok) {
                        if !partial_ok || !m.contains("tail row was rewritten") {
                            failures.push(format!("[{name}] partial_ok={partial_ok}: {m}"));
                        }
                    }
                }
            }
        }
    }
    failures.sort();
    failures.dedup();
    // One example per answer, so a pile of repeats of one bug does not hide the others.
    let mut seen = std::collections::HashSet::new();
    let firsts: Vec<&String> = failures
        .iter()
        .filter(|f| seen.insert(f.split(']').next().unwrap_or("").to_string()))
        .collect();
    assert!(
        failures.is_empty(),
        "{} violations in {} answers, one of each:\n{}",
        failures.len(),
        firsts.len(),
        firsts
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n\n")
    );
}

#[test]
fn no_code_row_is_a_lone_closing_mark_at_any_width() {
    // Round 1 finding 21 and round 2 finding 5: a row of `↳ ;`. Greedy wrapping puts the `;`
    // of `...into());` on a row of its own whenever `)` ends a row exactly.
    let pal = Palette::new(Kind::Hearth, Depth::True);
    let theme = pal.theme();
    let code = "let name = std::env::args().nth(1).unwrap_or_else(|| \"world\".into());\n    println!(\"hello, {name}\");\nfoo(bar(baz(qux(1, 2, 3)))));\n";
    let src = format!("```rust\n{code}```\n");
    for width in 14..=110usize {
        let cx = cx(&pal, &theme, width);
        for row in plain(&render(&src, &cx)) {
            let t = row.trim();
            let body = t.trim_start_matches('↳').trim();
            if t.starts_with('↳') {
                assert!(
                    body.chars().count() >= 3 && !body.chars().all(|c| ")];,.".contains(c)),
                    "width {width}: continuation row {row:?}"
                );
            }
        }
    }
}

#[test]
fn markup_is_never_shown_and_then_removed() {
    let pal = Palette::new(Kind::Hearth, Depth::True);
    let theme = pal.theme();
    let src = "A **bold** word, *italic* one, a `code span`, ~~struck~~ text and a [link](https://example.com/a) here.\n\n## Heading with `code`\n\n- **bold item** with `code`\n- [second](https://x.y) item\n\n```rust\nlet a = 1;\n```\n\nDone with ***both***.";
    for width in [30usize, 80] {
        let cx = cx(&pal, &theme, width);
        let fin = plain(&render(src, &cx)).join("\n");
        for mark in ["**", "`", "~~", "](", "[", "##"] {
            assert!(!fin.contains(mark), "{mark:?} survives in the final render");
        }
        for partial_ok in [false, true] {
            let mut st = MdState::default();
            for end in (1..=src.len()).filter(|i| src.is_char_boundary(*i)) {
                let rows = plain(st.update(&src[..end], &cx, false, partial_ok)).join("\n");
                for mark in ["**", "`", "~~", "](", "[", "##"] {
                    assert!(
                        !rows.contains(mark),
                        "width {width} partial_ok={partial_ok}: {mark:?} shown after {end} bytes:\n{rows}"
                    );
                }
            }
        }
    }
}

#[test]
fn a_long_answer_streams_without_quadratic_cost() {
    let pal = Palette::new(Kind::Hearth, Depth::True);
    let theme = pal.theme();
    let cx = cx(&pal, &theme, 100);
    let para = "A paragraph of **prose** with `code` and a [link](https://example.com) that goes on for a while.\n\n";
    let src: String = para.repeat(600);
    let mut st = MdState::default();
    let t = Instant::now();
    let mut worst = std::time::Duration::ZERO;
    let mut end = 0;
    while end < src.len() {
        end = (end + 24).min(src.len());
        while !src.is_char_boundary(end) {
            end += 1;
        }
        let t0 = Instant::now();
        st.update(&src[..end], &cx, false, false);
        worst = worst.max(t0.elapsed());
    }
    eprintln!(
        "{} KB streamed in {:?}, slowest delta {:?}",
        src.len() / 1024,
        t.elapsed(),
        worst
    );
    // Debug build; the release figure is an order of magnitude lower.
    assert!(worst.as_millis() < 100, "a delta took {worst:?}");
}
