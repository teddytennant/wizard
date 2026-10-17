// OWNER: anim (the animation layer: clock, per-cell colour formulas, wake schedule)
//! Everything that moves in Grok Build, as functions of a clock.
//!
//! Grok recolours single cells every tick (the rail, the diamond, the logo), so none of it can be
//! a widget style. The transcript and welcome code store *what* animates (an accent, a row
//! index) and ask this module for the colour at paint time, already blended against the cell's
//! own background. The clock is a plain function of elapsed time, so a test freezes it and a
//! screenshot is reproducible; the event loop only asks `Wake` for the next instant something
//! changes, so an idle screen never wakes.

use std::time::{Duration, Instant};

use ratatui::style::Color;

use crate::theme::{blend, Theme};

/// One animation tick, as measured (34.0 to 34.2 ms; the source says 33 plus handler time).
pub const TICK: Duration = Duration::from_millis(34);
/// The welcome logo repaints at 12 fps.
pub const SHIMMER_FPS: f64 = 12.0;
const TAU: f32 = std::f32::consts::TAU;

/// Spinner glyphs of the turn status row, one step per 4 ticks (136 ms).
pub const SPINNER: [char; 8] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧'];
/// Dock and pane dot spinner, same cadence.
pub const DOTS: [char; 8] = ['⋅', ':', '⸬', '⁙', '⋅', ':', '⸬', '⁙'];

/// Seconds since the app started, or a frozen value for tests and screenshots.
#[derive(Clone, Copy, Debug)]
pub struct Clock {
    start: Instant,
    frozen: Option<Duration>,
}

impl Clock {
    pub fn new() -> Self {
        Clock {
            start: Instant::now(),
            frozen: None,
        }
    }

    pub fn freeze(&mut self, at: Duration) {
        self.frozen = Some(at);
    }

    pub fn is_frozen(&self) -> bool {
        self.frozen.is_some()
    }

    pub fn elapsed(&self) -> Duration {
        self.frozen.unwrap_or_else(|| self.start.elapsed())
    }

    pub fn secs(&self) -> f64 {
        self.elapsed().as_secs_f64()
    }

    /// Animation tick counter: `elapsed / 34 ms`.
    pub fn tick(&self) -> u64 {
        (self.elapsed().as_millis() / TICK.as_millis()) as u64
    }

    /// The instant a clock reading `d` falls at.
    pub fn instant_at(&self, d: Duration) -> Instant {
        self.start + d
    }

    /// The instant at which tick `n` begins.
    pub fn at_tick(&self, n: u64) -> Instant {
        self.start + TICK * n as u32
    }

    /// First instant strictly after now whose tick is a multiple of `every`.
    pub fn next_tick_multiple(&self, every: u64) -> Instant {
        let t = self.tick() / every + 1;
        self.at_tick(t * every)
    }

    /// First instant after now at which `floor(secs * fps)` changes.
    pub fn next_frame(&self, fps: f64) -> Instant {
        let f = (self.secs() * fps).floor() + 1.0;
        self.start + Duration::from_secs_f64(f / fps)
    }
}

impl Default for Clock {
    fn default() -> Self {
        Self::new()
    }
}

/// `sin^2(tick*speed + (row/rows)*2π)`; the running rail uses speed 0.15 and 32 rows.
pub fn wave(tick: u64, row: usize, speed: f32, rows: f32) -> f32 {
    let a = tick as f32 * speed + (row as f32 / rows) * TAU;
    a.sin().powi(2)
}

pub fn pulse(tick: u64, speed: f32) -> f32 {
    (tick as f32 * speed).sin().powi(2)
}

/// Rail and bullet colour of a running block at `row` of the block.
pub fn rail(bg: Color, accent: Color, tick: u64, row: usize) -> Color {
    blend(bg, accent, wave(tick, row, 0.15, 32.0))
}

/// The `◆` that replaces the spinner while the agent waits on the user.
pub fn waiting_diamond(t: &Theme, tick: u64) -> Color {
    blend(t.bg_base, t.accent_user, 0.3 + 0.7 * pulse(tick, 0.08))
}

pub fn spinner(tick: u64) -> char {
    SPINNER[((tick / 4) % 8) as usize]
}

pub fn dots(tick: u64) -> char {
    DOTS[((tick / 4) % 8) as usize]
}

const BAND: f32 = 0.38;
const CYCLE: f32 = 4.0;
const SWEEP_FRAC: f32 = 0.32;
const SHINE: f32 = 0.33;
const PULSE: f32 = 0.06;
const PULSE_SECS: f32 = 5.0;

/// How far `glyph = lerp(gray, text_primary, o)` is pushed at a diagonal position `diag`
/// (0 bottom-left, 1 top-right) `secs` after the welcome screen appeared.
pub fn shine_opacity(diag: f32, secs: f32) -> f32 {
    let p = (secs % CYCLE) / CYCLE;
    let q = (p / SWEEP_FRAC).min(1.0);
    let band_pos = -BAND + q * (1.0 + 2.0 * BAND);
    let pulse = PULSE * (0.5 - 0.5 * (TAU * secs / PULSE_SECS).cos());
    let d = (diag - band_pos).abs();
    let shine = if d < BAND {
        0.5 * (1.0 + (std::f32::consts::PI * d / BAND).cos())
    } else {
        0.0
    };
    (pulse + SHINE * shine).clamp(0.0, 1.0)
}

/// Colour of the logo glyph at (`row`, `col`) of a `rows` x `cols` art.
pub fn logo_color(t: &Theme, row: usize, col: usize, rows: usize, cols: usize, secs: f64) -> Color {
    let diag = (col + (rows - 1 - row)) as f32 / (cols + rows) as f32;
    let o = shine_opacity(diag, (secs % 20.0) as f32);
    blend(t.gray, t.text_primary, o)
}

/// What the loop has to wake for. Everything is a deadline; `None` parks the loop.
#[derive(Clone, Copy, Debug, Default)]
pub struct Wake {
    next: Option<Instant>,
}

impl Wake {
    pub fn none() -> Self {
        Wake { next: None }
    }

    pub fn at(&mut self, t: Instant) {
        self.next = Some(self.next.map_or(t, |n| n.min(t)));
    }

    pub fn get(&self) -> Option<Instant> {
        self.next
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rail_wave_reproduces_the_thinking_rail_range() {
        // fg 20 + 68*b: 20 at b=0, 88 at b=1
        let t = Theme::groknight();
        let mut lo = 255u8;
        let mut hi = 0u8;
        for tick in 0..60 {
            let Color::Rgb(r, _, _) = rail(t.bg_base, t.gray_dim, tick, 3) else {
                panic!()
            };
            lo = lo.min(r);
            hi = hi.max(r);
        }
        assert!(lo <= 22 && hi >= 86, "{lo}..{hi}");
    }

    #[test]
    fn rail_rows_step_by_a_fixed_phase() {
        // 46-thinking-live rows 7..12 read 212121 2c2c2c 3a3a3a 464646 505050 575757
        let t = Theme::groknight();
        let want = [0x21u8, 0x2c, 0x3a, 0x46, 0x50, 0x57];
        // find the tick whose phase is closest to the capture's first row (angle about 0.45)
        let best = (0..64u64)
            .min_by_key(|&k| {
                (0..6)
                    .map(|r| {
                        let Color::Rgb(v, _, _) = rail(t.bg_base, t.gray_dim, k, r) else {
                            panic!()
                        };
                        (v as i32 - want[r] as i32).abs()
                    })
                    .sum::<i32>()
            })
            .unwrap();
        let err: i32 = (0..6)
            .map(|r| {
                let Color::Rgb(v, _, _) = rail(t.bg_base, t.gray_dim, best, r) else {
                    panic!()
                };
                (v as i32 - want[r] as i32).abs()
            })
            .sum();
        assert!(err < 12, "tick {best} err {err}");
    }

    #[test]
    fn waiting_diamond_stays_in_range() {
        let t = Theme::groknight();
        for tick in 0..100 {
            let Color::Rgb(r, _, _) = waiting_diamond(&t, tick) else {
                panic!()
            };
            assert!((74..=200).contains(&r), "{r}");
        }
    }

    #[test]
    fn logo_rests_between_108_and_115_and_peaks_near_154() {
        let t = Theme::groknight();
        let (mut lo, mut hi) = (255u8, 0u8);
        for ms in (0..20_000).step_by(83) {
            for row in 0..7 {
                for col in 0..14 {
                    let Color::Rgb(r, _, _) = logo_color(&t, row, col, 7, 14, ms as f64 / 1000.0)
                    else {
                        panic!()
                    };
                    lo = lo.min(r);
                    hi = hi.max(r);
                }
            }
        }
        assert_eq!(lo, 108);
        assert!((150..=156).contains(&hi), "{hi}");
    }

    #[test]
    fn spinner_steps_every_four_ticks() {
        assert_eq!(spinner(0), '⠋');
        assert_eq!(spinner(3), '⠋');
        assert_eq!(spinner(4), '⠙');
        assert_eq!(spinner(32), '⠋');
    }
}
