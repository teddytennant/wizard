//! Colour depth: what the terminal can show, and a pass over a frame that reduces it.
//!
//! Themes are written in RGB. A terminal that does not say it has 24-bit colour
//! (`COLORTERM=truecolor` or `24bit`, or a `-direct` TERM) gets the nearest of the 256 xterm
//! colours instead of `38;2;r;g;b`, which such terminals misread. `NO_COLOR` (and `TERM=dumb`)
//! drop every colour and keep the attributes. `PI_TRUE_COLOR=1` or `0` forces either way.

use std::sync::atomic::{AtomicU8, Ordering};

use ratatui::buffer::Buffer;
use ratatui::style::Color;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Depth {
    True = 0,
    Ansi256 = 1,
    Mono = 2,
}

/// The depth the environment asks for; `get` reads one variable (empty counts as unset).
pub fn from_vars(get: impl Fn(&str) -> Option<String>) -> Depth {
    let var = |k: &str| get(k).filter(|v| !v.is_empty());
    if var("NO_COLOR").is_some() || var("TERM").as_deref() == Some("dumb") {
        return Depth::Mono;
    }
    match var("PI_TRUE_COLOR").as_deref() {
        Some("1" | "true") => return Depth::True,
        Some("0" | "false") => return Depth::Ansi256,
        _ => {}
    }
    let direct = var("TERM").is_some_and(|t| t.ends_with("-direct"));
    if direct || matches!(var("COLORTERM").as_deref(), Some("truecolor" | "24bit")) {
        Depth::True
    } else {
        Depth::Ansi256
    }
}

pub fn from_env() -> Depth {
    from_vars(|k| std::env::var(k).ok())
}

static CURRENT: AtomicU8 = AtomicU8::new(0);

/// The depth this process draws at (set once at startup; truecolor until then).
pub fn set(d: Depth) {
    CURRENT.store(d as u8, Ordering::Relaxed);
}

pub fn current() -> Depth {
    match CURRENT.load(Ordering::Relaxed) {
        1 => Depth::Ansi256,
        2 => Depth::Mono,
        _ => Depth::True,
    }
}

/// The nearest xterm 256-colour index: the best of the 6x6x6 cube and the grey ramp.
pub fn rgb_to_256(r: u8, g: u8, b: u8) -> u8 {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let near = |v: u8| {
        LEVELS
            .iter()
            .enumerate()
            .min_by_key(|(_, l)| (i32::from(**l) - i32::from(v)).abs())
            .map_or(0, |(i, _)| i)
    };
    let (ri, gi, bi) = (near(r), near(g), near(b));
    let cube = (LEVELS[ri], LEVELS[gi], LEVELS[bi]);
    let cube_idx = 16 + 36 * ri + 6 * gi + bi;
    let avg = (u32::from(r) + u32::from(g) + u32::from(b)) / 3;
    let gi = ((avg.saturating_sub(8) + 5) / 10).min(23) as u8;
    let gv = 8 + 10 * gi;
    let dist = |c: (u8, u8, u8)| {
        let d = |a: u8, b: u8| (i32::from(a) - i32::from(b)).pow(2);
        d(c.0, r) + d(c.1, g) + d(c.2, b)
    };
    if dist((gv, gv, gv)) < dist(cube) {
        232 + gi
    } else {
        cube_idx as u8
    }
}

/// `c` at depth `d`: an RGB colour becomes an indexed one, and under `Mono` every colour goes.
pub fn reduce_color(c: Color, d: Depth) -> Color {
    match (d, c) {
        (Depth::True, c) => c,
        (Depth::Mono, _) => Color::Reset,
        (Depth::Ansi256, Color::Rgb(r, g, b)) => Color::Indexed(rgb_to_256(r, g, b)),
        (Depth::Ansi256, c) => c,
    }
}

/// Reduce every cell of a finished frame in place.
pub fn reduce(buf: &mut Buffer, d: Depth) {
    if d == Depth::True {
        return;
    }
    for cell in &mut buf.content {
        cell.fg = reduce_color(cell.fg, d);
        cell.bg = reduce_color(cell.bg, d);
        cell.underline_color = reduce_color(cell.underline_color, d);
    }
}

/// The SGR parameter for `c` as a foreground or background at depth `d`, if it has one.
pub fn sgr_color(c: Color, fg: bool, d: Depth) -> Option<String> {
    let base = if fg { 38 } else { 48 };
    match reduce_color(c, d) {
        Color::Rgb(r, g, b) => Some(format!("{base};2;{r};{g};{b}")),
        Color::Indexed(n) => Some(format!("{base};5;{n}")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;
    use ratatui::style::Style;

    fn env(pairs: &'static [(&'static str, &'static str)]) -> Depth {
        from_vars(|k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        })
    }

    #[test]
    fn the_environment_picks_the_depth() {
        assert_eq!(env(&[("COLORTERM", "truecolor")]), Depth::True);
        assert_eq!(env(&[("COLORTERM", "24bit")]), Depth::True);
        assert_eq!(env(&[("TERM", "xterm-direct")]), Depth::True);
        assert_eq!(env(&[("TERM", "xterm-256color")]), Depth::Ansi256);
        assert_eq!(env(&[]), Depth::Ansi256);
        assert_eq!(env(&[("TERM", "linux")]), Depth::Ansi256);
        assert_eq!(
            env(&[("NO_COLOR", "1"), ("COLORTERM", "truecolor")]),
            Depth::Mono
        );
        assert_eq!(env(&[("TERM", "dumb")]), Depth::Mono);
        assert_eq!(
            env(&[("NO_COLOR", "")]),
            Depth::Ansi256,
            "an empty NO_COLOR is unset"
        );
        assert_eq!(env(&[("PI_TRUE_COLOR", "1")]), Depth::True);
        assert_eq!(
            env(&[("COLORTERM", "truecolor"), ("PI_TRUE_COLOR", "0")]),
            Depth::Ansi256
        );
    }

    #[test]
    fn rgb_lands_on_the_nearest_xterm_colour() {
        assert_eq!(rgb_to_256(0, 0, 0), 16);
        assert_eq!(rgb_to_256(255, 255, 255), 231);
        assert_eq!(rgb_to_256(255, 0, 0), 196);
        assert_eq!(rgb_to_256(0, 255, 0), 46);
        // a mid grey is on the grey ramp, not the cube
        assert!((232..=255).contains(&rgb_to_256(128, 128, 128)));
        assert_eq!(rgb_to_256(95, 135, 175), 16 + 36 + 6 * 2 + 3);
    }

    #[test]
    fn a_frame_is_reduced_in_place() {
        let mut b = Buffer::empty(Rect::new(0, 0, 2, 1));
        b.set_string(
            0,
            0,
            "ab",
            Style::default()
                .fg(Color::Rgb(255, 0, 0))
                .bg(Color::Rgb(0, 0, 0)),
        );
        let mut mono = b.clone();
        reduce(&mut b, Depth::Ansi256);
        assert_eq!(b[(0, 0)].fg, Color::Indexed(196));
        assert_eq!(b[(0, 0)].bg, Color::Indexed(16));
        reduce(&mut mono, Depth::Mono);
        assert_eq!(mono[(1, 0)].fg, Color::Reset);
        assert_eq!(
            sgr_color(Color::Rgb(255, 0, 0), true, Depth::True).as_deref(),
            Some("38;2;255;0;0")
        );
        assert_eq!(
            sgr_color(Color::Rgb(255, 0, 0), false, Depth::Ansi256).as_deref(),
            Some("48;5;196")
        );
        assert_eq!(sgr_color(Color::Rgb(255, 0, 0), true, Depth::Mono), None);
    }
}
