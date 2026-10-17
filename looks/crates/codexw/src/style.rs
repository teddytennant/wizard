// OWNER: renderer
//! Colour derivation (spec A.7) and the shimmer (A.8).
//!
//! Codex draws with ANSI palette entries plus dim and bold. The only computed colours are the
//! tint behind user messages and the composer, the table rule, the accent on a light terminal
//! and the shimmer; all of them come from the terminal's own default fg and bg, read once at
//! startup by the probe.

use std::cell::Cell;
use std::time::Instant;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

pub type Rgb = (u8, u8, u8);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorLevel {
    TrueColor,
    Ansi256,
    /// 16 colours or unknown: tints disappear.
    Ansi16,
}

impl ColorLevel {
    /// What `supports_color` decides from the environment.
    pub fn from_env() -> Self {
        let colorterm = std::env::var("COLORTERM")
            .unwrap_or_default()
            .to_ascii_lowercase();
        let term = std::env::var("TERM").unwrap_or_default();
        if colorterm == "truecolor"
            || colorterm == "24bit"
            || std::env::var_os("WT_SESSION").is_some()
        {
            ColorLevel::TrueColor
        } else if term.contains("256color") {
            ColorLevel::Ansi256
        } else {
            ColorLevel::Ansi16
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    pub fg: Option<Rgb>,
    pub bg: Option<Rgb>,
    pub level: ColorLevel,
}

impl Default for Palette {
    fn default() -> Self {
        Palette {
            fg: None,
            bg: None,
            level: ColorLevel::TrueColor,
        }
    }
}

thread_local! {
    static PALETTE: Cell<Palette> = Cell::new(Palette::default());
}

/// The palette for this thread. The UI runs on one thread; tests set their own.
pub fn palette() -> Palette {
    PALETTE.with(|p| p.get())
}

pub fn set_palette(p: Palette) {
    PALETTE.with(|c| c.set(p));
}

pub fn is_light((r, g, b): Rgb) -> bool {
    0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32 > 128.0
}

/// `fg * alpha + bg * (1 - alpha)` per channel, truncating, f32 maths like Codex.
pub fn blend(fg: Rgb, bg: Rgb, alpha: f32) -> Rgb {
    let m = |f: u8, b: u8| (f as f32 * alpha + b as f32 * (1.0 - alpha)) as u8;
    (m(fg.0, bg.0), m(fg.1, bg.1), m(fg.2, bg.2))
}

fn srgb_to_lab((r, g, b): Rgb) -> (f32, f32, f32) {
    let lin = |c: u8| {
        let c = c as f32 / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    let (r, g, b) = (lin(r), lin(g), lin(b));
    let x = r * 0.4124564 + g * 0.3575761 + b * 0.1804375;
    let y = r * 0.2126729 + g * 0.7151522 + b * 0.0721750;
    let z = r * 0.0193339 + g * 0.119_192 + b * 0.9503041;
    let f = |t: f32| {
        if t > 0.008856 {
            t.cbrt()
        } else {
            7.787 * t + 16.0 / 116.0
        }
    };
    let (fx, fy, fz) = (f(x / 0.95047), f(y / 1.0), f(z / 1.08883));
    (116.0 * fy - 16.0, 500.0 * (fx - fy), 200.0 * (fy - fz))
}

/// CIE76 distance.
pub fn perceptual_distance(a: Rgb, b: Rgb) -> f32 {
    let (l1, a1, b1) = srgb_to_lab(a);
    let (l2, a2, b2) = srgb_to_lab(b);
    ((l1 - l2).powi(2) + (a1 - a2).powi(2) + (b1 - b2).powi(2)).sqrt()
}

/// xterm 256-colour table entry (16..=255 only; the first 16 are themed by the user).
pub fn xterm_rgb(i: u8) -> Rgb {
    if i >= 232 {
        let v = 8 + 10 * (i - 232);
        return (v, v, v);
    }
    let n = i - 16;
    let lv = |c: u8| if c == 0 { 0 } else { 55 + 40 * c };
    (lv(n / 36), lv((n / 6) % 6), lv(n % 6))
}

impl Palette {
    pub fn new(fg: Option<Rgb>, bg: Option<Rgb>, level: ColorLevel) -> Self {
        Palette { fg, bg, level }
    }

    pub fn light_bg(&self) -> bool {
        self.bg.is_some_and(is_light)
    }

    /// Truecolor passes through, 256 colours pick the nearest of 16..=255, anything less is
    /// `Reset` (no tint at all).
    pub fn best_color(&self, target: Rgb) -> Color {
        match self.level {
            ColorLevel::TrueColor => Color::Rgb(target.0, target.1, target.2),
            ColorLevel::Ansi256 => {
                let mut best = 16u8;
                let mut d = f32::MAX;
                for i in 16..=255u8 {
                    let dd = perceptual_distance(target, xterm_rgb(i));
                    if dd < d {
                        d = dd;
                        best = i;
                    }
                }
                Color::Indexed(best)
            }
            ColorLevel::Ansi16 => Color::Reset,
        }
    }

    /// The tint behind user messages, the composer and plan proposals. `None` without a probed bg.
    pub fn user_message_bg(&self) -> Option<Color> {
        let bg = self.bg?;
        let top = if is_light(bg) {
            (0, 0, 0)
        } else {
            (255, 255, 255)
        };
        let alpha = if is_light(bg) { 0.04 } else { 0.12 };
        Some(self.best_color(blend(top, bg, alpha)))
    }

    pub fn user_message_style(&self) -> Style {
        match self.user_message_bg() {
            Some(c) if c != Color::Reset => Style::default().bg(c),
            _ => Style::default(),
        }
    }

    /// Accent for selected and active controls.
    pub fn accent(&self) -> Style {
        if self.light_bg() {
            Style::default().fg(self.best_color((0, 95, 135))).bold()
        } else {
            Style::default().fg(Color::Cyan).bold()
        }
    }

    /// Picker selected row marker and title colour.
    pub fn picker_highlight(&self) -> Color {
        if self.light_bg() {
            Color::Magenta
        } else {
            Color::Yellow
        }
    }

    /// Overlay of white (dark bg) or black (light bg) at `alpha`, for the picker rows.
    pub fn overlay_bg(&self, dark_alpha: f32, light_alpha: f32) -> Option<Color> {
        let bg = self.bg?;
        let c = if is_light(bg) {
            blend((0, 0, 0), bg, light_alpha)
        } else {
            blend((255, 255, 255), bg, dark_alpha)
        };
        Some(self.best_color(c))
    }

    /// Table rule: 20 percent of the way from the terminal bg to its fg.
    pub fn table_rule_style(&self) -> Style {
        match (self.fg, self.bg) {
            (Some(fg), Some(bg)) => {
                let c = self.best_color(blend(fg, bg, 0.20));
                if c == Color::Reset {
                    Style::default().dim()
                } else {
                    Style::default().fg(c)
                }
            }
            _ => Style::default().dim(),
        }
    }
}

// ---- shimmer ------------------------------------------------------------------------------

thread_local! {
    static START: Instant = Instant::now();
}

/// Seconds since the first call on this thread; every shimmer shares it so they stay in phase.
pub fn process_elapsed() -> f32 {
    START.with(|s| s.elapsed().as_secs_f32())
}

/// The sweeping highlight over `text` at `elapsed` seconds (spec A.8.1).
pub fn shimmer_spans_at(text: &str, elapsed: f32, p: &Palette) -> Vec<Span<'static>> {
    let chars: Vec<char> = text.chars().collect();
    let padding = 10usize;
    let period = chars.len() + padding * 2;
    let sweep = 2.0f32;
    let pos = (((elapsed % sweep) / sweep) * period as f32) as usize;
    let band_half = 5.0f32;
    let truecolor = p.level == ColorLevel::TrueColor;
    let base = p.fg.unwrap_or((128, 128, 128));
    let hi = p.bg.unwrap_or((255, 255, 255));
    chars
        .iter()
        .enumerate()
        .map(|(i, ch)| {
            let i_pos = (i + padding) as f32;
            let dist = (i_pos - pos as f32).abs();
            let t = if dist <= band_half {
                0.5 * (1.0 + (std::f32::consts::PI * dist / band_half).cos())
            } else {
                0.0
            };
            let style = if truecolor {
                let (r, g, b) = blend(hi, base, t.clamp(0.0, 1.0) * 0.9);
                Style::default()
                    .fg(Color::Rgb(r, g, b))
                    .add_modifier(Modifier::BOLD)
            } else if t < 0.2 {
                Style::default().add_modifier(Modifier::DIM)
            } else if t < 0.6 {
                Style::default()
            } else {
                Style::default().add_modifier(Modifier::BOLD)
            };
            Span::styled(ch.to_string(), style)
        })
        .collect()
}

pub fn shimmer_spans(text: &str) -> Vec<Span<'static>> {
    shimmer_spans_at(text, process_elapsed(), &palette())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pal(fg: Rgb, bg: Rgb) -> Palette {
        Palette::new(Some(fg), Some(bg), ColorLevel::TrueColor)
    }

    #[test]
    fn user_tint_matches_the_captures() {
        assert_eq!(
            pal((230, 230, 230), (0, 0, 0)).user_message_bg(),
            Some(Color::Rgb(30, 30, 30))
        );
        assert_eq!(
            pal((26, 26, 26), (255, 255, 255)).user_message_bg(),
            Some(Color::Rgb(244, 244, 244))
        );
        let p256 = Palette::new(Some((230, 230, 230)), Some((0, 0, 0)), ColorLevel::Ansi256);
        assert_eq!(p256.user_message_bg(), Some(Color::Indexed(234)));
        let p16 = Palette::new(Some((230, 230, 230)), Some((0, 0, 0)), ColorLevel::Ansi16);
        assert_eq!(p16.user_message_style(), Style::default());
    }

    #[test]
    fn table_rule_values() {
        assert_eq!(
            pal((230, 230, 230), (0, 0, 0)).table_rule_style().fg,
            Some(Color::Rgb(46, 46, 46))
        );
        assert_eq!(
            pal((26, 26, 26), (255, 255, 255)).table_rule_style().fg,
            Some(Color::Rgb(209, 209, 209))
        );
        assert_eq!(
            pal((255, 255, 255), (0, 0, 0)).table_rule_style().fg,
            Some(Color::Rgb(51, 51, 51))
        );
    }

    #[test]
    fn overlay_alphas() {
        let p = pal((230, 230, 230), (0, 0, 0));
        assert_eq!(p.overlay_bg(0.055, 0.04), Some(Color::Rgb(14, 14, 14)));
        assert_eq!(p.overlay_bg(0.14, 0.08), Some(Color::Rgb(35, 35, 35)));
    }

    #[test]
    fn shimmer_band_matches_slow_03() {
        // Band centred on the 'k' of "Working" (index 3): pos = 3 + 10 = 13.
        let p = pal((230, 230, 230), (0, 0, 0));
        let period = 7 + 20;
        let elapsed = (13.0 / period as f32) * 2.0 + 0.001;
        let spans = shimmer_spans_at("Working", elapsed, &p);
        let vals: Vec<u8> = spans
            .iter()
            .map(|s| match s.style.fg {
                Some(Color::Rgb(r, _, _)) => r,
                _ => 0,
            })
            .collect();
        assert_eq!(vals, vec![158, 94, 42, 23, 42, 94, 158]);
    }

    #[test]
    fn shimmer_on_light_background() {
        let p = pal((26, 26, 26), (255, 255, 255));
        let period = 7 + 20;
        let elapsed = (13.0 / period as f32) * 2.0 + 0.001;
        let spans = shimmer_spans_at("Working", elapsed, &p);
        let r = |i: usize| match spans[i].style.fg {
            Some(Color::Rgb(r, _, _)) => r,
            _ => 0,
        };
        assert_eq!((r(3), r(2), r(1), r(0)), (232, 212, 160, 97));
    }

    #[test]
    fn xterm_table_picks_234_for_30() {
        assert_eq!(xterm_rgb(234), (28, 28, 28));
        assert_eq!(xterm_rgb(16), (0, 0, 0));
        assert_eq!(xterm_rgb(231), (255, 255, 255));
    }
}
