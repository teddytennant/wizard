//! Cell-for-cell comparison with a capture of the real jcode (`tests/reference/*.ansi`):
//! character, foreground, background and the attributes bold, dim, italic and underline.
//! `tools/compare.py` does the same over every scene; this is the part that runs in `cargo test`.

use std::path::PathBuf;

use ratatui::style::{Color, Modifier};
use tuikit::testing::TestTerminal;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Col {
    Default,
    Rgb(u8, u8, u8),
    Idx(u8),
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Px {
    ch: char,
    fg: Col,
    bg: Col,
    attrs: u8,
}

const BOLD: u8 = 1;
const DIM: u8 = 2;
const ITALIC: u8 = 4;
const UNDER: u8 = 8;

const BLANK: Px = Px {
    ch: ' ',
    fg: Col::Default,
    bg: Col::Default,
    attrs: 0,
};

/// A `tmux capture-pane -e -p` dump as rows of cells, each `width` wide.
pub fn parse(text: &str, width: usize) -> Vec<Vec<Px>> {
    let (mut fg, mut bg, mut attrs) = (Col::Default, Col::Default, 0u8);
    let mut rows = Vec::new();
    for line in text.split('\n') {
        let mut cells = Vec::new();
        let ch: Vec<char> = line.chars().collect();
        let mut i = 0;
        while i < ch.len() {
            if ch[i] == '\x1b' && ch.get(i + 1) == Some(&'[') {
                let mut j = i + 2;
                let mut body = String::new();
                while j < ch.len() && !ch[j].is_ascii_alphabetic() {
                    body.push(ch[j]);
                    j += 1;
                }
                if ch.get(j) == Some(&'m') {
                    let p: Vec<u32> = body
                        .split([';', ':'])
                        .map(|x| x.parse().unwrap_or(0))
                        .collect();
                    let mut k = 0;
                    while k < p.len() {
                        match p[k] {
                            0 => {
                                fg = Col::Default;
                                bg = Col::Default;
                                attrs = 0;
                            }
                            1 => attrs |= BOLD,
                            2 => attrs |= DIM,
                            3 => attrs |= ITALIC,
                            4 => attrs |= UNDER,
                            22 => attrs &= !(BOLD | DIM),
                            23 => attrs &= !ITALIC,
                            24 => attrs &= !UNDER,
                            39 => fg = Col::Default,
                            49 => bg = Col::Default,
                            c @ (38 | 48) if p.get(k + 1) == Some(&2) && k + 4 < p.len() => {
                                let v = Col::Rgb(p[k + 2] as u8, p[k + 3] as u8, p[k + 4] as u8);
                                if c == 38 {
                                    fg = v
                                } else {
                                    bg = v
                                }
                                k += 4;
                            }
                            c @ (38 | 48) if p.get(k + 1) == Some(&5) && k + 2 < p.len() => {
                                let v = Col::Idx(p[k + 2] as u8);
                                if c == 38 {
                                    fg = v
                                } else {
                                    bg = v
                                }
                                k += 2;
                            }
                            c @ 30..=37 => fg = Col::Idx(c as u8 - 30),
                            _ => {}
                        }
                        k += 1;
                    }
                }
                i = j + 1;
                continue;
            }
            cells.push(Px {
                ch: ch[i],
                fg,
                bg,
                attrs,
            });
            if tuikit::width::display_width(&ch[i].to_string()) == 2 {
                cells.push(BLANK);
            }
            i += 1;
        }
        cells.resize(width, BLANK);
        rows.push(cells);
    }
    rows
}

fn col_of(c: Color) -> Col {
    match c {
        Color::Rgb(r, g, b) => Col::Rgb(r, g, b),
        Color::Indexed(n) => Col::Idx(n),
        Color::Red => Col::Idx(1),
        _ => Col::Default,
    }
}

fn attrs_of(m: Modifier) -> u8 {
    let mut a = 0;
    for (f, v) in [
        (Modifier::BOLD, BOLD),
        (Modifier::DIM, DIM),
        (Modifier::ITALIC, ITALIC),
        (Modifier::UNDERLINED, UNDER),
    ] {
        if m.contains(f) {
            a |= v;
        }
    }
    a
}

fn screen(t: &TestTerminal) -> Vec<Vec<Px>> {
    let buf = t.buffer();
    (0..buf.area.height)
        .map(|y| {
            let mut row = Vec::new();
            let mut x = 0;
            while x < buf.area.width {
                let c = &buf[(x, y)];
                let sym = c.symbol();
                let w = tuikit::width::display_width(sym).max(1) as u16;
                row.push(Px {
                    ch: sym.chars().next().unwrap_or(' '),
                    fg: col_of(c.fg),
                    bg: col_of(c.bg),
                    attrs: attrs_of(c.modifier),
                });
                row.extend(std::iter::repeat_n(BLANK, w as usize - 1));
                x += w;
            }
            row
        })
        .collect()
}

fn text(row: &[Px]) -> String {
    row.iter().map(|p| p.ch).collect()
}

/// Clear the info widgets wizard has no data for, and move a prompt row's end marker to the
/// last column, where it is when there is no scrollbar.
fn normalise(rows: &mut [Vec<Px>]) {
    let border = Col::Rgb(70, 70, 80);
    for y in 0..rows.len() {
        let width = rows[y].len();
        for x in 0..width {
            if rows[y][x].ch != '╭' || rows[y][x].fg != border {
                continue;
            }
            let Some(close) = (x..width).find(|&i| rows[y][i].ch == '╮') else {
                continue;
            };
            let mut y2 = y;
            while y2 + 1 < rows.len()
                && rows[y2][x].ch != '╰'
                && matches!(rows[y2 + 1][x].ch, '│' | '╰')
            {
                y2 += 1;
            }
            // what the box hides is unknown, so those cells match anything
            for row in rows.iter_mut().take(y2 + 1).skip(y) {
                for p in row.iter_mut().take(close + 1).skip(x) {
                    *p = Px { ch: '\0', ..BLANK };
                }
            }
        }
    }
    for row in rows.iter_mut() {
        let w = row.len();
        if w > 2 && row[w - 2].ch == '│' && row[w - 2].fg == Col::Rgb(138, 180, 248) {
            row[w - 1] = row[w - 2].clone();
            row[w - 2] = BLANK;
        }
    }
}

pub fn reference(name: &str, width: usize) -> Vec<Vec<Px>> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/reference")
        .join(format!("{name}.ansi"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reference {name}: {e}"));
    let mut rows = parse(&text, width);
    normalise(&mut rows);
    rows
}

/// Where a row stops being comparable: jcode's copy badges (`[Alt] [⇧] [C]`), which the look
/// does not draw, and numbers that differ from run to run (elapsed time, token counts).
fn volatile_from(row: &str) -> Option<usize> {
    let chars: Vec<char> = row.chars().collect();
    let mut text: String = chars.iter().collect();
    let badge = text.find(" [Alt]");
    if let Some(at) = badge {
        text.truncate(at);
    }
    let trimmed = text.trim_end_matches([' ', '│', '╷', '╵', '•', '\0']);
    if let Some(body) = trimmed.strip_suffix(" tok") {
        let n = body.chars().count();
        let start = body
            .chars()
            .rev()
            .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == 'k')
            .count();
        return Some(n - start);
    }
    if let Some(at) = badge {
        return Some(text[..at].chars().count());
    }
    let lead = chars.iter().take_while(|c| **c == ' ').count();
    if trimmed.contains('↑')
        || (trimmed
            .trim_start()
            .starts_with(|c: char| c.is_ascii_digit())
            && trimmed.contains('s'))
    {
        return chars
            .iter()
            .enumerate()
            .skip(lead)
            .find(|(_, c)| c.is_ascii_digit())
            .map(|(i, _)| i);
    }
    None
}

/// Differences between the screen and the capture over the block that starts at the first row
/// containing `from` and ends at the first row containing `to` (inclusive). The scrollbar
/// thumb is left out: its size follows the height of the header.
pub fn diff_block(name: &str, t: &TestTerminal, from: &str, to: &str) -> Vec<String> {
    let mut ours = screen(t);
    normalise(&mut ours);
    let width = ours.first().map_or(0, Vec::len);
    let theirs = reference(name, width);
    let find = |rows: &[Vec<Px>], needle: &str, start: usize| {
        rows.iter()
            .enumerate()
            .skip(start)
            .find(|(_, r)| text(r).contains(needle))
            .map(|(i, _)| i)
    };
    let (Some(a0), Some(b0)) = (find(&ours, from, 0), find(&theirs, from, 0)) else {
        return vec![format!("{name}: no row with {from:?} on one side")];
    };
    let (Some(a1), Some(b1)) = (find(&ours, to, a0), find(&theirs, to, b0)) else {
        return vec![format!("{name}: no row with {to:?} on one side")];
    };
    let mut out = Vec::new();
    if a1 - a0 != b1 - b0 {
        out.push(format!(
            "{name}: the block is {} rows here and {} in the capture",
            a1 - a0 + 1,
            b1 - b0 + 1
        ));
        return out;
    }
    for i in 0..=(a1 - a0) {
        let (a, b) = (&ours[a0 + i], &theirs[b0 + i]);
        let limit = volatile_from(&text(b)).unwrap_or(width);
        for x in 0..limit.min(width) {
            let thumb = Col::Rgb(136, 148, 172);
            if x + 1 == width && (a[x].fg == thumb || b[x].fg == thumb) {
                continue;
            }
            if b[x].ch == '\0' {
                continue;
            }
            let (mut p, mut q) = (a[x].clone(), b[x].clone());
            if p.ch == ' ' && q.ch == ' ' {
                p.fg = q.fg;
                p.attrs &= !(BOLD | DIM | ITALIC);
                q.attrs &= !(BOLD | DIM | ITALIC);
            }
            if p != q {
                out.push(format!("row {i} col {x}: capture {q:?} screen {p:?}"));
            }
        }
    }
    out
}

pub fn assert_block(name: &str, t: &TestTerminal, from: &str, to: &str) {
    let d = diff_block(name, t, from, to);
    assert!(
        d.is_empty(),
        "{name}: {} cells differ, first:\n  {}",
        d.len(),
        d.iter().take(8).cloned().collect::<Vec<_>>().join("\n  ")
    );
}
