//! ANSI handling for tool output: strip escape sequences, or turn SGR into styled spans.
//!
//! Only SGR (`ESC [ ... m`) is interpreted. Every other CSI, OSC, and two-byte escape is
//! dropped, because cursor movement in captured output would otherwise corrupt the layout.

use crate::width::{grapheme_width, tab_width};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;

#[derive(Debug, PartialEq, Eq)]
enum Tok<'a> {
    Text(&'a str),
    Sgr(Vec<Vec<i64>>),
}

fn tokenize(s: &str) -> Vec<Tok<'_>> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    let mut text_start = 0;
    while i < b.len() {
        if b[i] != 0x1b {
            i += 1;
            continue;
        }
        if text_start < i {
            out.push(Tok::Text(&s[text_start..i]));
        }
        i += 1;
        if i >= b.len() {
            text_start = i;
            break;
        }
        match b[i] {
            b'[' => {
                // CSI: parameters 0x30..=0x3f, intermediates 0x20..=0x2f, final 0x40..=0x7e.
                let p0 = i + 1;
                let mut j = p0;
                while j < b.len() && (0x30..=0x3f).contains(&b[j]) {
                    j += 1;
                }
                let params_end = j;
                while j < b.len() && (0x20..=0x2f).contains(&b[j]) {
                    j += 1;
                }
                if j < b.len() && (0x40..=0x7e).contains(&b[j]) {
                    if b[j] == b'm' && params_end == j {
                        out.push(Tok::Sgr(parse_params(&s[p0..params_end])));
                    }
                    i = j + 1;
                } else {
                    i = j; // truncated sequence, drop what we saw
                }
            }
            b']' | b'P' | b'X' | b'^' | b'_' => {
                // OSC / DCS / SOS / PM / APC: runs to BEL or ST (ESC \).
                let mut j = i + 1;
                while j < b.len() {
                    if b[j] == 0x07 {
                        j += 1;
                        break;
                    }
                    if b[j] == 0x1b && j + 1 < b.len() && b[j + 1] == b'\\' {
                        j += 2;
                        break;
                    }
                    j += 1;
                }
                i = j;
            }
            0x20..=0x2f => {
                // nF escape: intermediates then one final byte.
                let mut j = i;
                while j < b.len() && (0x20..=0x2f).contains(&b[j]) {
                    j += 1;
                }
                i = (j + 1).min(b.len());
            }
            _ => i += 1, // two-byte escape such as ESC c or ESC 7
        }
        // Resume text at the next char boundary.
        while i < b.len() && !s.is_char_boundary(i) {
            i += 1;
        }
        text_start = i;
    }
    if text_start < b.len() {
        out.push(Tok::Text(&s[text_start..]));
    }
    out
}

/// One entry per `;` group; a group holds several numbers when it used the ITU colon form
/// (`38:2::r:g:b`, `4:3`).
fn parse_params(p: &str) -> Vec<Vec<i64>> {
    if p.is_empty() {
        return vec![vec![0]];
    }
    p.split(';')
        .map(|g| g.split(':').map(|x| x.parse().unwrap_or(0)).collect())
        .collect()
}

/// Remove every escape sequence and control byte except `\n` and `\t`.
pub fn strip(s: &str) -> String {
    if !s.contains('\x1b') && !s.chars().any(|c| c.is_control() && c != '\n' && c != '\t') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    for t in tokenize(s) {
        if let Tok::Text(t) = t {
            out.extend(
                t.chars()
                    .filter(|&c| !c.is_control() || c == '\n' || c == '\t'),
            );
        }
    }
    out
}

fn rgb(r: i64, g: i64, b: i64) -> Color {
    Color::Rgb(
        r.clamp(0, 255) as u8,
        g.clamp(0, 255) as u8,
        b.clamp(0, 255) as u8,
    )
}

fn set_color(style: &mut Style, fg: bool, c: Color) {
    if fg {
        style.fg = Some(c);
    } else {
        style.bg = Some(c);
    }
}

fn apply_sgr(style: &mut Style, base: Style, groups: &[Vec<i64>]) {
    let mut gi = 0;
    while gi < groups.len() {
        let group = &groups[gi];
        gi += 1;
        let p = group[0];
        if (p == 38 || p == 48) && group.len() > 1 {
            // Colon form, everything is inside this group.
            match group[1] {
                2 if group.len() >= 5 => set_color(
                    style,
                    p == 38,
                    rgb(
                        group[group.len() - 3],
                        group[group.len() - 2],
                        group[group.len() - 1],
                    ),
                ),
                5 if group.len() >= 3 => {
                    set_color(style, p == 38, Color::Indexed(group[2].clamp(0, 255) as u8))
                }
                _ => {}
            }
            continue;
        }
        if p == 38 || p == 48 {
            // Semicolon form takes its operands from the following groups.
            let next = |k: usize| groups.get(gi + k).map(|g| g[0]);
            match next(0) {
                Some(5) => {
                    if let Some(n) = next(1) {
                        set_color(style, p == 38, Color::Indexed(n.clamp(0, 255) as u8));
                    }
                    gi += 2;
                }
                Some(2) => {
                    if let (Some(r), Some(g), Some(b)) = (next(1), next(2), next(3)) {
                        set_color(style, p == 38, rgb(r, g, b));
                    }
                    gi += 4;
                }
                _ => gi = groups.len(),
            }
            continue;
        }
        match p {
            0 => *style = base,
            1 => *style = style.add_modifier(Modifier::BOLD),
            2 => *style = style.add_modifier(Modifier::DIM),
            3 => *style = style.add_modifier(Modifier::ITALIC),
            4 => {
                // `4:0` turns underline off, other colon values pick a style we draw as plain.
                if group.get(1) == Some(&0) {
                    *style = style.remove_modifier(Modifier::UNDERLINED);
                } else {
                    *style = style.add_modifier(Modifier::UNDERLINED);
                }
            }
            // Blink and conceal are not for tool output. Conceal in particular hides text from
            // the reader that the model still reads (`ESC[8m` ... instructions ... `ESC[0m`).
            5 | 6 | 8 => {}
            7 => *style = style.add_modifier(Modifier::REVERSED),
            9 => *style = style.add_modifier(Modifier::CROSSED_OUT),
            21 => *style = style.add_modifier(Modifier::UNDERLINED),
            22 => *style = style.remove_modifier(Modifier::BOLD | Modifier::DIM),
            23 => *style = style.remove_modifier(Modifier::ITALIC),
            24 => *style = style.remove_modifier(Modifier::UNDERLINED),
            27 => *style = style.remove_modifier(Modifier::REVERSED),
            29 => *style = style.remove_modifier(Modifier::CROSSED_OUT),
            30..=37 => style.fg = Some(ansi16((p - 30) as u8)),
            40..=47 => style.bg = Some(ansi16((p - 40) as u8)),
            90..=97 => style.fg = Some(ansi16((p - 90 + 8) as u8)),
            100..=107 => style.bg = Some(ansi16((p - 100 + 8) as u8)),
            39 => style.fg = base.fg,
            49 => style.bg = base.bg,
            _ => {}
        }
    }
}

/// The colour as an exact RGB, when the terminal's own palette does not decide it.
fn exact_rgb(c: Color) -> Option<(u8, u8, u8)> {
    match c {
        Color::Rgb(r, g, b) => Some((r, g, b)),
        Color::Indexed(n) if n >= 16 => crate::theme::rgb_of(c),
        _ => None,
    }
}

fn luminance((r, g, b): (u8, u8, u8)) -> f64 {
    let f = |v: u8| {
        let v = f64::from(v) / 255.0;
        if v <= 0.03928 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * f(r) + 0.7152 * f(g) + 0.0722 * f(b)
}

/// Least WCAG contrast an exact-colour pair needs to count as readable.
const MIN_CONTRAST: f64 = 2.0;

fn unreadable(fg: Option<Color>, bg: Option<Color>) -> bool {
    let (Some(f), Some(b)) = (fg, bg) else {
        return false;
    };
    if f == b {
        return true;
    }
    match (exact_rgb(f), exact_rgb(b)) {
        (Some(f), Some(b)) => {
            let (lf, lb) = (luminance(f), luminance(b));
            (lf.max(lb) + 0.05) / (lf.min(lb) + 0.05) < MIN_CONTRAST
        }
        _ => false,
    }
}

/// `style` with a colour pair the reader could not see put back to `base`.
fn legible(style: Style, base: Style, surface: Option<Color>) -> Style {
    let bg = style.bg.or(base.bg).or(surface);
    let fg = style.fg.or(base.fg);
    let hidden = if style.add_modifier.contains(Modifier::REVERSED) {
        // Reversed, the text is drawn in the background colour on the foreground.
        unreadable(bg, fg)
    } else {
        unreadable(fg, bg)
    };
    if !hidden {
        return style;
    }
    Style {
        fg: base.fg,
        bg: base.bg,
        add_modifier: style.add_modifier - Modifier::REVERSED,
        ..style
    }
}

fn ansi16(n: u8) -> Color {
    match n {
        0 => Color::Black,
        1 => Color::Red,
        2 => Color::Green,
        3 => Color::Yellow,
        4 => Color::Blue,
        5 => Color::Magenta,
        6 => Color::Cyan,
        7 => Color::Gray,
        8 => Color::DarkGray,
        9 => Color::LightRed,
        10 => Color::LightGreen,
        11 => Color::LightYellow,
        12 => Color::LightBlue,
        13 => Color::LightMagenta,
        14 => Color::LightCyan,
        _ => Color::White,
    }
}

/// Convert text with SGR sequences into lines of styled spans. Style carries across lines the
/// way a terminal would. `base` is what `ESC[0m` resets to. Tabs expand to the next 4-column
/// stop, lone `\r` keeps only the text after it (progress bars), `\b` erases the previous
/// character (overstrike), and other control bytes are dropped.
pub fn to_lines(s: &str, base: Style) -> Vec<Line<'static>> {
    to_lines_on(s, base, None)
}

/// [`to_lines`] for lines drawn over `surface`: a colour the output picks that the reader could
/// not tell from the background (black on black, or an RGB too close to it) is dropped, so
/// output cannot carry text that only the model sees.
pub fn to_lines_on(s: &str, base: Style, surface: Option<Color>) -> Vec<Line<'static>> {
    let mut style = base;
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut cur = String::new();
    let mut col = 0usize;

    let flush = |cur: &mut String, spans: &mut Vec<Span<'static>>, style: Style| {
        if !cur.is_empty() {
            spans.push(Span::styled(
                std::mem::take(cur),
                legible(style, base, surface),
            ));
        }
    };

    for tok in tokenize(s) {
        match tok {
            Tok::Sgr(params) => {
                flush(&mut cur, &mut spans, style);
                apply_sgr(&mut style, base, &params);
            }
            Tok::Text(t) => {
                let mut it = t.graphemes(true).peekable();
                while let Some(g) = it.next() {
                    match g {
                        "\n" | "\r\n" => {
                            flush(&mut cur, &mut spans, style);
                            lines.push(Line::from(std::mem::take(&mut spans)));
                            col = 0;
                        }
                        "\r" => {
                            if it.peek().is_some_and(|n| *n == "\n") {
                                continue;
                            }
                            // Overwrite from column 0: drop the line so far.
                            cur.clear();
                            spans.clear();
                            col = 0;
                        }
                        "\t" => {
                            let n = tab_width() - col % tab_width();
                            cur.extend(std::iter::repeat_n(' ', n));
                            col += n;
                        }
                        "\x08" => {
                            if let Some(last) = cur.graphemes(true).next_back().map(str::to_owned) {
                                cur.truncate(cur.len() - last.len());
                                col = col.saturating_sub(grapheme_width(&last));
                            } else if let Some(sp) = spans.last_mut() {
                                let mut c = sp.content.to_string();
                                if let Some(last) = c.graphemes(true).next_back().map(str::to_owned)
                                {
                                    c.truncate(c.len() - last.len());
                                    col = col.saturating_sub(grapheme_width(&last));
                                }
                                if c.is_empty() {
                                    spans.pop();
                                } else {
                                    sp.content = c.into();
                                }
                            }
                        }
                        g if g.chars().all(|c| c.is_control()) => {}
                        g => {
                            cur.push_str(g);
                            col += grapheme_width(g);
                        }
                    }
                }
            }
        }
    }
    flush(&mut cur, &mut spans, style);
    // A trailing newline does not start a new empty line; text without one still counts.
    if !spans.is_empty() || lines.is_empty() {
        lines.push(Line::from(spans));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(l: &Line<'_>) -> String {
        l.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn strip_removes_sgr_osc_and_cursor_moves() {
        let s = "\x1b[31mred\x1b[0m \x1b]8;;http://x\x07link\x1b]8;;\x07 \x1b[2Kgone\x1b[10;5H!";
        assert_eq!(strip(s), "red link gone!");
        assert_eq!(strip("plain\ttext\nnext"), "plain\ttext\nnext");
        assert_eq!(strip("a\x1b"), "a");
        assert_eq!(strip("a\x1b[3"), "a");
    }

    #[test]
    fn sgr_colours_and_modifiers() {
        let lines = to_lines(
            "\x1b[1;31mboom\x1b[0m ok \x1b[38;2;1;2;3mrgb\x1b[48;5;200mx",
            Style::default(),
        );
        assert_eq!(lines.len(), 1);
        let sp = &lines[0].spans;
        assert_eq!(sp[0].content, "boom");
        assert_eq!(sp[0].style.fg, Some(Color::Red));
        assert!(sp[0].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(sp[1].style, Style::default());
        assert_eq!(sp[2].style.fg, Some(Color::Rgb(1, 2, 3)));
        assert_eq!(sp[3].style.bg, Some(Color::Indexed(200)));
        assert_eq!(sp[3].style.fg, Some(Color::Rgb(1, 2, 3)));
    }

    #[test]
    fn style_carries_across_lines() {
        let lines = to_lines("\x1b[32ma\nb\x1b[0m\nc", Style::default());
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[1].spans[0].style.fg, Some(Color::Green));
        assert_eq!(lines[2].spans[0].style.fg, None);
    }

    #[test]
    fn reset_returns_to_base_style() {
        let base = Style::new().fg(Color::Rgb(9, 9, 9));
        let lines = to_lines("\x1b[31ma\x1b[0mb\x1b[39mc", base);
        assert_eq!(lines[0].spans[1].style, base);
        assert_eq!(lines[0].spans[2].style.fg, base.fg);
    }

    #[test]
    fn carriage_return_keeps_last_overwrite() {
        let lines = to_lines("10%\r50%\r100%\ndone\r\nx", Style::default());
        assert_eq!(text(&lines[0]), "100%");
        assert_eq!(text(&lines[1]), "done");
        assert_eq!(text(&lines[2]), "x");
    }

    #[test]
    fn tabs_expand_to_stops_and_backspace_overstrikes() {
        let lines = to_lines("a\tb\nN\x08Nx", Style::default());
        assert_eq!(text(&lines[0]), "a   b");
        assert_eq!(text(&lines[1]), "Nx");
    }

    #[test]
    fn trailing_newline_does_not_add_empty_line() {
        assert_eq!(to_lines("a\n", Style::default()).len(), 1);
        assert_eq!(to_lines("", Style::default()).len(), 1);
        assert_eq!(to_lines("a\n\n", Style::default()).len(), 2);
    }

    #[test]
    fn colon_form_truecolor_with_colorspace_slot() {
        let lines = to_lines("\x1b[38:2::10:20:30mx", Style::default());
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Rgb(10, 20, 30)));
        let lines = to_lines("\x1b[38:2:10:20:30mx", Style::default());
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Rgb(10, 20, 30)));
    }

    #[test]
    fn conceal_and_blink_are_ignored() {
        let l = to_lines(
            "a\x1b[8mSECRET\x1b[0m b\x1b[5mblink\x1b[6m!\x1b[0m",
            Style::default(),
        );
        assert_eq!(text(&l[0]), "aSECRET bblink!");
        for sp in &l[0].spans {
            assert!(
                !sp.style
                    .add_modifier
                    .intersects(Modifier::HIDDEN | Modifier::SLOW_BLINK | Modifier::RAPID_BLINK),
                "{sp:?}"
            );
        }
    }

    #[test]
    fn text_the_reader_cannot_tell_from_the_background_is_put_back_to_base() {
        let base = Style::default().fg(Color::Rgb(180, 170, 150));
        let surface = Color::Rgb(27, 23, 20);
        let fg_of = |input: &str| {
            let l = to_lines_on(input, base, Some(surface));
            l[0].spans.last().unwrap().style
        };
        // black on black, the same named colour twice
        assert_eq!(fg_of("\x1b[30;40mhidden").fg, base.fg);
        assert_eq!(fg_of("\x1b[30;40mhidden").bg, base.bg);
        // an RGB that is the surface itself, or a hair off it
        assert_eq!(fg_of("\x1b[38;2;27;23;20mhidden").fg, base.fg);
        assert_eq!(fg_of("\x1b[38;2;30;26;22mhidden").fg, base.fg);
        // reversed, the text is drawn in the background colour
        let s = fg_of("\x1b[7;38;2;27;23;20mhidden");
        assert!(!s.add_modifier.contains(Modifier::REVERSED), "{s:?}");
        // colours a person can read stay
        assert_eq!(fg_of("\x1b[31mred").fg, Some(Color::Red));
        assert_eq!(
            fg_of("\x1b[38;2;255;100;0morange").fg,
            Some(Color::Rgb(255, 100, 0))
        );
        assert_eq!(fg_of("\x1b[34mls dir").fg, Some(Color::Blue));
        // an explicit background that makes the pair readable stays too
        assert_eq!(fg_of("\x1b[30;47mbadge").bg, Some(Color::Gray));
    }
}
