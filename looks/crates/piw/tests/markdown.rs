//! Pi's markdown against the real captures: `reference/pi/*/30b-md-full.ansi` carries the `[[md]]`
//! message rendered at three sizes. Cells are compared for character, foreground, bold, italic,
//! underline and strike; backgrounds do not exist in assistant text.

use piw::theme::PiTheme;
use piw::ui::markdown_theme::render;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use std::path::PathBuf;

pub const MD: &str = "# Heading one\n\nSome **bold**, some *italic*, some ~~strike~~, and `inline code` in a paragraph with a [link](https://example.com/docs) and a bare https://example.com/bare url.\n\n## Heading two\n\n### Heading three\n\n- first bullet\n- second bullet with `code`\n  - nested bullet\n  - another nested\n1. numbered one\n2. numbered two\n\n> A block quote that\n> spans two lines.\n\n```rust\nfn main() {\n    // say hi\n    let n: u32 = 42;\n    println!(\"hi {}\", n);\n}\n```\n\n```python\ndef f(x):\n    return [i * 2 for i in range(x)]  # doubles\n```\n\n```\nplain fence with no language\n```\n\n| name | qty | note |\n|------|----:|------|\n| apple | 3 | red |\n| banana | 12 | yellow and long enough to wrap in a narrow terminal window |\n\n---\n\nLast paragraph after a rule.\n";

#[derive(Clone, Debug, PartialEq)]
struct Cell {
    ch: char,
    fg: Option<(u8, u8, u8)>,
    bold: bool,
    italic: bool,
    under: bool,
    strike: bool,
}

/// Parse the SGR dump `tmux capture-pane -e` wrote into rows of cells.
fn parse_ansi(text: &str) -> Vec<Vec<Cell>> {
    let mut rows = Vec::new();
    let mut fg: Option<(u8, u8, u8)> = None;
    let (mut bold, mut italic, mut under, mut strike) = (false, false, false, false);
    for line in text.split('\n') {
        let mut cells = Vec::new();
        let chars: Vec<char> = line.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            if chars[i] == '\x1b' && chars.get(i + 1) == Some(&'[') {
                let mut j = i + 2;
                let mut params = String::new();
                while j < chars.len() && chars[j] != 'm' {
                    params.push(chars[j]);
                    j += 1;
                }
                let p: Vec<i64> = params.split(';').map(|x| x.parse().unwrap_or(0)).collect();
                let mut k = 0;
                while k < p.len() {
                    match p[k] {
                        0 => {
                            fg = None;
                            bold = false;
                            italic = false;
                            under = false;
                            strike = false;
                        }
                        1 => bold = true,
                        3 => italic = true,
                        4 => under = true,
                        9 => strike = true,
                        22 => bold = false,
                        23 => italic = false,
                        24 => under = false,
                        29 => strike = false,
                        39 => fg = None,
                        38 if p.get(k + 1) == Some(&2) => {
                            fg = Some((p[k + 2] as u8, p[k + 3] as u8, p[k + 4] as u8));
                            k += 4;
                        }
                        48 if p.get(k + 1) == Some(&2) => k += 4,
                        _ => {}
                    }
                    k += 1;
                }
                i = j + 1;
                continue;
            }
            cells.push(Cell {
                ch: chars[i],
                fg,
                bold,
                italic,
                under,
                strike,
            });
            i += 1;
        }
        rows.push(cells);
    }
    rows
}

fn reference(dir: &str) -> Vec<Vec<Cell>> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../reference/pi")
        .join(dir)
        .join("30b-md-full.ansi");
    parse_ansi(&std::fs::read_to_string(p).expect("reference capture"))
}

fn ours(lines: &[Line<'static>]) -> Vec<Vec<Cell>> {
    lines
        .iter()
        .map(|l| {
            let mut v = Vec::new();
            for s in &l.spans {
                let st: Style = s.style;
                let fg = match st.fg {
                    Some(Color::Rgb(r, g, b)) => Some((r, g, b)),
                    _ => None,
                };
                for ch in s.content.chars() {
                    v.push(Cell {
                        ch,
                        fg,
                        bold: st.add_modifier.contains(Modifier::BOLD),
                        italic: st.add_modifier.contains(Modifier::ITALIC),
                        under: st.add_modifier.contains(Modifier::UNDERLINED),
                        strike: st.add_modifier.contains(Modifier::CROSSED_OUT),
                    });
                }
            }
            v
        })
        .collect()
}

/// The reference rows that hold the assistant message: after the user block (a row of the text
/// `full markdown [[md]]`), up to the first rule of the dock.
fn message_rows(rows: &[Vec<Cell>]) -> Vec<Vec<Cell>> {
    let text = |r: &Vec<Cell>| r.iter().map(|c| c.ch).collect::<String>();
    let user = rows
        .iter()
        .position(|r| text(r).contains("[[md]]"))
        .unwrap();
    let rule = rows
        .iter()
        .position(|r| text(r).starts_with("────────────────────"))
        .unwrap();
    // user text row, its padding row, then the assistant message's blank row
    rows[user + 3..rule].to_vec()
}

fn compare(dir: &str, width: u16) {
    let theme = PiTheme::dark();
    let got = ours(&render(MD.trim(), width - 2, &theme, Style::default()));
    let want = message_rows(&reference(dir));
    // drop the one column of padding and trailing blank rows of the capture
    let strip = |r: &Vec<Cell>| -> Vec<Cell> {
        let mut v: Vec<Cell> = r.iter().skip(1).cloned().collect();
        while v.last().is_some_and(|c| c.ch == ' ') {
            v.pop();
        }
        v
    };
    let mut want: Vec<Vec<Cell>> = want.iter().map(strip).collect();
    while want.last().is_some_and(|r| r.is_empty()) {
        want.pop();
    }
    let got: Vec<Vec<Cell>> = got
        .iter()
        .map(|r| {
            let mut v = r.clone();
            while v.last().is_some_and(|c| c.ch == ' ') {
                v.pop();
            }
            v
        })
        .collect();
    let mut bad = Vec::new();
    for y in 0..want.len().max(got.len()) {
        let (a, b) = (want.get(y), got.get(y));
        let txt =
            |r: Option<&Vec<Cell>>| r.map_or(String::new(), |r| r.iter().map(|c| c.ch).collect());
        if txt(a) != txt(b) {
            bad.push(format!(
                "row {y}: text\n  want {:?}\n  got  {:?}",
                txt(a),
                txt(b)
            ));
            continue;
        }
        let (a, b) = (a.unwrap(), b.unwrap());
        for x in 0..a.len() {
            let (p, q) = (&a[x], &b[x]);
            if p.ch == ' ' {
                continue;
            }
            if p != q {
                bad.push(format!("row {y} col {x} {:?}: want {p:?} got {q:?}", p.ch));
            }
        }
    }
    assert!(
        bad.is_empty(),
        "{dir}: {} differences\n{}",
        bad.len(),
        bad.iter().take(12).cloned().collect::<Vec<_>>().join("\n")
    );
}

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn md_at_120_matches_the_capture() {
    compare("120x36", 120);
}

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn md_at_150_matches_the_capture() {
    compare("150x42", 150);
}

#[test]
#[cfg_attr(
    not(reference_captures),
    ignore = "needs the reference/ captures, not in this repository; see build.rs"
)]
fn md_at_80_matches_the_capture() {
    compare("80x24", 80);
}

fn plain(lines: &[Line<'static>]) -> Vec<String> {
    lines
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect()
}

#[test]
fn thinking_is_italic_in_one_colour_with_a_blank_between_paragraphs() {
    let t = PiTheme::dark();
    let base = t
        .fg(piw::theme::Tok::ThinkingText)
        .add_modifier(Modifier::ITALIC);
    let rows = render(
        "First paragraph.\n\nSecond paragraph of thinking.",
        118,
        &t,
        base,
    );
    assert_eq!(
        plain(&rows),
        ["First paragraph.", "", "Second paragraph of thinking."]
    );
    let c = ours(&rows);
    assert!(c[0]
        .iter()
        .all(|x| x.italic && x.fg == Some((0x96, 0xa0, 0xa4))));
    assert!(c[2]
        .iter()
        .all(|x| x.italic && x.fg == Some((0x96, 0xa0, 0xa4))));
}

#[test]
fn long_token_moves_to_its_own_row_and_breaks_at_the_width() {
    let t = PiTheme::dark();
    let src = format!(
        "A very long unbroken token: {}\n\nAnd a normal paragraph.",
        "x".repeat(300)
    );
    let rows = plain(&render(&src, 118, &t, Style::default()));
    assert_eq!(rows[0], "A very long unbroken token:");
    assert_eq!(rows[1], "x".repeat(118));
    assert_eq!(rows[2], "x".repeat(118));
    assert_eq!(rows[3], "x".repeat(64));
    assert_eq!(rows[4], "");
    assert_eq!(rows[5], "And a normal paragraph.");
}

#[test]
fn user_text_keeps_its_colour_under_bold_and_its_soft_breaks() {
    let t = PiTheme::dark();
    let base = t.fg(piw::theme::Tok::UserMessageText);
    let rows = render("multi\nline\n**bold** user text", 118, &t, base);
    assert_eq!(plain(&rows), ["multi", "line", "bold user text"]);
    let c = ours(&rows);
    assert!(c[2][..4]
        .iter()
        .all(|x| x.bold && x.fg == Some((0xde, 0xe0, 0xe1))));
    assert!(c[2][5..].iter().all(|x| !x.bold));
}

#[test]
fn user_messages_keep_list_markers_and_escapes() {
    let t = PiTheme::dark();
    let base = t.fg(piw::theme::Tok::UserMessageText);
    let rows = plain(&render(
        "3. three\n4. four\n\n\\*not italic\\*",
        118,
        &t,
        base,
    ));
    assert_eq!(rows, ["3. three", "4. four", "", "\\*not italic\\*"]);
    // assistant text numbers from the list start and drops the escapes
    let rows = plain(&render(
        "3. three\n4. four\n\n\\*not italic\\*",
        118,
        &t,
        Style::default(),
    ));
    assert_eq!(rows, ["3. three", "4. four", "", "*not italic*"]);
}

#[test]
fn streaming_partial_closing_fence_is_trimmed() {
    let t = PiTheme::dark();
    let rows = plain(&render("```rust\nfn x() {}\n``", 40, &t, Style::default()));
    assert_eq!(rows, ["```rust", "  fn x() {}", "```"]);
}

#[test]
fn table_falls_back_to_raw_text_when_too_narrow() {
    let t = PiTheme::dark();
    let rows = plain(&render(
        "| a | b |\n|---|---|\n| 1 | 2 |",
        8,
        &t,
        Style::default(),
    ));
    assert!(rows[0].starts_with("| a | b"), "{rows:?}");
    assert!(!rows.iter().any(|r| r.contains('┌')));
}

#[test]
fn empty_and_blank_input_render_nothing() {
    let t = PiTheme::dark();
    assert!(render("", 20, &t, Style::default()).is_empty());
    assert!(render("  \n ", 20, &t, Style::default()).is_empty());
}
