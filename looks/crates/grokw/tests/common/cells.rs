//! Cell-for-cell comparison with a reference capture (`reference/grok/**/*.ansi`), the Rust twin
//! of `tools/grokw-cmp.py`: character, foreground, background and the attributes bold, dim,
//! italic, underline, reverse and strike. A blank row a capture left without SGR is the screen
//! background, like the script reads it.

use std::path::PathBuf;

use ratatui::style::{Color, Modifier};
use tuikit::testing::TestTerminal;

type Rgb = (u8, u8, u8);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Col {
    Rgb(Rgb),
    Idx(u8),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Px {
    ch: char,
    fg: Col,
    bg: Col,
    attrs: u8,
}

const BOLD: u8 = 1;
const DIM: u8 = 2;
const ITALIC: u8 = 4;
const UNDER: u8 = 8;
const REV: u8 = 16;
const STRIKE: u8 = 32;
const DEFAULT_BG: Col = Col::Rgb((0x14, 0x14, 0x14));
const DEFAULT_FG: Col = Col::Rgb((0xe1, 0xe1, 0xe1));

/// Rows of cells and, per row, the background the cells after the last printed one have: a
/// trailing `48;..m` is how a capture says a trimmed run of blanks is not on the default.
fn parse(text: &str, dbg: Col) -> Vec<(Vec<Px>, Col)> {
    let (mut fg, mut bg, mut attrs) = (DEFAULT_FG, dbg, 0u8);
    let mut rows = Vec::new();
    for line in text.split('\n') {
        let mut cells = Vec::new();
        let mut ends_in_sgr = false;
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
                                fg = DEFAULT_FG;
                                bg = dbg;
                                attrs = 0;
                            }
                            1 => attrs |= BOLD,
                            2 => attrs |= DIM,
                            3 => attrs |= ITALIC,
                            4 => attrs |= UNDER,
                            7 => attrs |= REV,
                            9 => attrs |= STRIKE,
                            22 => attrs &= !(BOLD | DIM),
                            23 => attrs &= !ITALIC,
                            24 => attrs &= !UNDER,
                            27 => attrs &= !REV,
                            29 => attrs &= !STRIKE,
                            39 => fg = DEFAULT_FG,
                            49 => bg = dbg,
                            c @ (38 | 48) if p.get(k + 1) == Some(&2) && k + 4 < p.len() => {
                                let v = Col::Rgb((p[k + 2] as u8, p[k + 3] as u8, p[k + 4] as u8));
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
                            _ => {}
                        }
                        k += 1;
                    }
                }
                i = j + 1;
                ends_in_sgr = true;
                continue;
            }
            ends_in_sgr = false;
            cells.push(Px {
                ch: ch[i],
                fg,
                bg,
                attrs,
            });
            i += 1;
        }
        rows.push((cells, if ends_in_sgr { bg } else { dbg }));
    }
    rows
}

fn col_of(c: Color, default: Col) -> Col {
    match c {
        Color::Rgb(r, g, b) => Col::Rgb((r, g, b)),
        Color::Indexed(n) => Col::Idx(n),
        _ => default,
    }
}

fn attrs_of(m: Modifier) -> u8 {
    let mut a = 0;
    for (f, v) in [
        (Modifier::BOLD, BOLD),
        (Modifier::DIM, DIM),
        (Modifier::ITALIC, ITALIC),
        (Modifier::UNDERLINED, UNDER),
        (Modifier::REVERSED, REV),
        (Modifier::CROSSED_OUT, STRIKE),
    ] {
        if m.contains(f) {
            a |= v;
        }
    }
    a
}

/// What to leave out of a comparison.
#[derive(Default)]
pub struct Skip {
    /// Rows not compared.
    pub rows: Vec<u16>,
    /// Rows `a..=b` not compared.
    pub spans: Vec<(u16, u16)>,
    /// Characters whose foreground is not compared (animated cells).
    pub fg_of: String,
    pub no_attrs: bool,
    /// What a cell the capture leaves unpainted shows: the theme's screen background.
    pub bg: Option<Color>,
    /// Row of the capture that row 0 of the screen is compared with (a short viewport at the
    /// bottom of a taller capture).
    pub offset: u16,
}

pub fn reference_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../reference/grok")
        .join(format!("{name}.ansi"))
}

/// Differences between `t` and `reference/grok/<name>.ansi`, one line each, empty when equal.
pub fn diff(name: &str, t: &TestTerminal, skip: &Skip) -> Vec<String> {
    let text = std::fs::read_to_string(reference_path(name))
        .unwrap_or_else(|e| panic!("reference {name}: {e}"));
    let dbg = skip.bg.map_or(DEFAULT_BG, |c| col_of(c, DEFAULT_BG));
    let want = parse(&text, dbg);
    let buf = t.buffer();
    let mut out = Vec::new();
    for y in 0..buf.area.height {
        if skip.rows.contains(&y) || skip.spans.iter().any(|&(a, b)| y >= a && y <= b) {
            continue;
        }
        let row = want.get((y + skip.offset) as usize);
        let mut x = 0u16;
        while x < buf.area.width {
            let c = &buf[(x, y)];
            let sym = c.symbol();
            let w = tuikit::width::display_width(sym).max(1) as u16;
            let ours = Px {
                ch: sym.chars().next().unwrap_or(' '),
                fg: col_of(c.fg, DEFAULT_FG),
                bg: col_of(c.bg, DEFAULT_BG),
                attrs: attrs_of(c.modifier),
            };
            let theirs = row
                .and_then(|(cells, _)| cells.get(x as usize))
                .copied()
                .unwrap_or(Px {
                    ch: ' ',
                    fg: DEFAULT_FG,
                    bg: row.map_or(dbg, |(_, end)| *end),
                    attrs: 0,
                });
            let mut a = ours;
            let mut b = theirs;
            if skip.fg_of.contains(b.ch) {
                a.fg = b.fg;
            }
            if skip.no_attrs {
                a.attrs = 0;
                b.attrs = 0;
            }
            // a blank cell shows no foreground, so its colour and attributes do not matter
            if a.ch == ' ' && b.ch == ' ' {
                a.fg = b.fg;
                a.attrs &= !(BOLD | DIM | ITALIC);
                b.attrs &= !(BOLD | DIM | ITALIC);
            }
            if a != b {
                out.push(format!("({y},{x}) want {b:?} got {a:?}"));
            }
            x += w;
        }
    }
    out
}

/// Panic with the first few differences when the screen is not the capture.
pub fn assert_cells(name: &str, t: &TestTerminal, skip: &Skip) {
    let d = diff(name, t, skip);
    assert!(
        d.is_empty(),
        "{name}: {} cells differ, first:\n  {}",
        d.len(),
        d.iter().take(8).cloned().collect::<Vec<_>>().join("\n  ")
    );
}
