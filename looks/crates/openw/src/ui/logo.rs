// OWNER: home (logo rows and wordmark)
//! The wordmark in opencode's 4-row slot: left half muted, right half bright, `_ ^ ~ ,` markers
//! turned into shadow cells. The geometry and marker rules are opencode's `logo.ts`; the letters
//! are ours (`wiz` + `ard`).

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use tuikit::paint::put_str;
use tuikit::theme::{rgb_of, Theme};

pub const HEIGHT: u16 = 4;
/// Width of the slot opencode's logo takes; the home layout centers the wordmark in it.
pub const SLOT_WIDTH: u16 = 39;

const LEFT: [&str; 4] = [
    "      ▀     ",
    "█   █ █ ▀▀▀█",
    "█ █ █ █  ▄▀ ",
    "▀▀▀▀▀ ▀ ▀▀▀▀",
];
const RIGHT: [&str; 4] = [
    "             █",
    "█▀▀█ █▀▀▀ █▀▀█",
    "█▀▀█ █    █__█",
    "▀  ▀ ▀    ▀▀▀▀",
];

/// Display width of the left half; the right half starts one cell after it.
const LEFT_WIDTH: u16 = 12;

/// Display width of the wordmark: 12 + 1 + 14.
pub const WIDTH: u16 = 27;

/// `bg + (fg - bg) * a` per channel, the logo's shadow.
pub fn tint(bg: Color, fg: Color, a: f32) -> Color {
    match (rgb_of(bg), rgb_of(fg)) {
        (Some((br, bg_, bb)), Some((fr, fg_, fb))) => {
            let m = |b: u8, f: u8| {
                (b as f32 + (f as f32 - b as f32) * a)
                    .round()
                    .clamp(0.0, 255.0) as u8
            };
            Color::Rgb(m(br, fr), m(bg_, fg_), m(bb, fb))
        }
        _ => fg,
    }
}

struct Half<'a> {
    row: &'a str,
    fg: Color,
    bold: bool,
}

fn half(buf: &mut Buffer, x: u16, y: u16, h: Half, theme: &Theme, clip: Rect) {
    let Half { row, fg, bold } = h;
    let bg = theme.background;
    let shadow = tint(bg, fg, 0.25);
    let base = Style::new().fg(fg);
    let base = if bold {
        base.add_modifier(Modifier::BOLD)
    } else {
        base
    };
    let mut cx = x;
    for ch in row.chars() {
        let s = ch.to_string();
        cx = match ch {
            '_' => put_str(buf, cx, y, " ", Style::new().bg(shadow), clip),
            '^' => put_str(buf, cx, y, "▀", base.bg(shadow), clip),
            '~' => put_str(buf, cx, y, "▀", Style::new().fg(shadow).bg(bg), clip),
            ',' => put_str(buf, cx, y, "▄", Style::new().fg(shadow).bg(bg), clip),
            ' ' => put_str(buf, cx, y, " ", Style::new().bg(bg), clip),
            _ => put_str(buf, cx, y, &s, base.bg(bg), clip),
        };
    }
}

/// Draw the wordmark with its top-left at (`x`, `y`).
pub fn draw(buf: &mut Buffer, x: u16, y: u16, theme: &Theme) {
    let clip = buf.area;
    for r in 0..HEIGHT as usize {
        let yy = y + r as u16;
        let left = Half {
            row: LEFT[r],
            fg: theme.text_muted,
            bold: false,
        };
        half(buf, x, yy, left, theme, clip);
        let right = Half {
            row: RIGHT[r],
            fg: theme.text,
            bold: true,
        };
        half(buf, x + LEFT_WIDTH + 1, yy, right, theme, clip);
    }
}

/// Plain text of the four rows (markers resolved), for tests and the exit epilogue.
pub fn plain_rows() -> Vec<String> {
    let clean = |s: &str| s.replace(['_', ','], " ").replace(['^', '~'], "▀");
    (0..HEIGHT as usize)
        .map(|r| format!("{} {}", clean(LEFT[r]), clean(RIGHT[r])))
        .collect()
}

/// ANSI text for the exit epilogue: left half bright gray, right half default foreground, with
/// the shadow colors opencode prints.
pub fn epilogue_rows() -> Vec<String> {
    let conv = |row: &str, shadow: u8, plain_prefix: &str| -> String {
        let mut out = String::new();
        for ch in row.chars() {
            match ch {
                '_' => out.push_str(&format!("\x1b[48;5;{shadow}m \x1b[0m{plain_prefix}")),
                '^' => out.push_str(&format!(
                    "\x1b[38;5;{shadow};48;5;{shadow}m▀\x1b[0m{plain_prefix}"
                )),
                '~' => out.push_str(&format!("\x1b[38;5;{shadow}m▀\x1b[0m{plain_prefix}")),
                c => out.push(c),
            }
        }
        out
    };
    (0..HEIGHT as usize)
        .map(|r| {
            format!(
                "\x1b[90m{}\x1b[0m {}",
                conv(LEFT[r], 235, "\x1b[90m"),
                conv(RIGHT[r], 238, "")
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuikit::testing::TestTerminal;

    #[test]
    fn markers_become_shadow_cells() {
        let theme = Theme::default_theme(tuikit::Mode::Dark);
        let mut t = TestTerminal::new(40, 6);
        t.draw(|b, _| draw(b, 2, 1, &theme));
        // `_` in the right half is a space on the tinted background.
        let c = t.cell(2 + 13 + 11, 3).unwrap();
        assert_eq!(c.symbol(), " ");
        assert_eq!(c.bg, tint(theme.background, theme.text, 0.25));
        assert_eq!(t.row(2), "  █   █ █ ▀▀▀█ █▀▀█ █▀▀▀ █▀▀█");
    }

    #[test]
    fn rows_are_four_and_fit_the_slot() {
        let r = plain_rows();
        assert_eq!(r.len(), 4);
        assert!(r.iter().all(|l| l.chars().count() as u16 <= WIDTH));
    }
}
