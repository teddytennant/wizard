//! Spinners from opencode: the braille dots in `component/spinner.tsx` (80 ms) and the
//! bidirectional block scanner under the prompt (`ui/spinner.ts` `createFrames` /
//! `createColors`, 40 ms, `blocks` style, inactive factor 0.6, min alpha 0.3).

use crate::theme::Theme;
use ratatui::style::{Color, Style};
use ratatui::text::Span;
use std::time::Duration;

pub const BRAILLE_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
pub const BRAILLE_INTERVAL: Duration = Duration::from_millis(80);
pub const SCANNER_INTERVAL: Duration = Duration::from_millis(40);
/// Shown instead of an animation when animations are off.
pub const STATIC_FRAME: &str = "⋯";

/// Braille frame for the time since the spinner started.
pub fn braille_frame(elapsed: Duration) -> &'static str {
    let i = (elapsed.as_millis() / BRAILLE_INTERVAL.as_millis()) as usize % BRAILLE_FRAMES.len();
    BRAILLE_FRAMES[i]
}

/// A braille spinner as a span in `color`.
pub fn braille_span(elapsed: Duration, color: Color) -> Span<'static> {
    Span::styled(braille_frame(elapsed), Style::new().fg(color))
}

#[derive(Clone, Copy)]
struct State {
    active: i64,
    holding: bool,
    hold_progress: i64,
    hold_total: i64,
    movement_progress: i64,
    movement_total: i64,
    forward: bool,
}

/// Knight Rider style scanner. `width` cells, a bright head with a fading trail that sweeps
/// right, holds, sweeps back, holds.
#[derive(Clone, Debug)]
pub struct Scanner {
    pub width: usize,
    pub hold_start: usize,
    pub hold_end: usize,
    pub color: Color,
    /// What the colours are blended over (the prompt panel).
    pub bg: Color,
    pub inactive_factor: f32,
    pub min_alpha: f32,
}

impl Scanner {
    /// The configuration the prompt uses.
    pub fn prompt(color: Color, bg: Color) -> Self {
        Self {
            width: 8,
            hold_start: 30,
            hold_end: 9,
            color,
            bg,
            inactive_factor: 0.6,
            min_alpha: 0.3,
        }
    }

    pub fn for_theme(color: Color, theme: &Theme) -> Self {
        Self::prompt(color, theme.background_element)
    }

    pub fn total_frames(&self) -> usize {
        self.width + self.hold_end + self.width.saturating_sub(1) + self.hold_start
    }

    /// Frame number for an elapsed time.
    pub fn frame_at(&self, elapsed: Duration) -> usize {
        let total = self.total_frames().max(1);
        (elapsed.as_millis() / SCANNER_INTERVAL.as_millis()) as usize % total
    }

    fn state(&self, frame: usize) -> State {
        let total = self.width as i64;
        let frame = frame as i64;
        let forward = total;
        let hold_end = self.hold_end as i64;
        let backward = total - 1;
        if frame < forward {
            State {
                active: frame,
                holding: false,
                hold_progress: 0,
                hold_total: 0,
                movement_progress: frame,
                movement_total: forward,
                forward: true,
            }
        } else if frame < forward + hold_end {
            State {
                active: total - 1,
                holding: true,
                hold_progress: frame - forward,
                hold_total: hold_end,
                movement_progress: 0,
                movement_total: 0,
                forward: true,
            }
        } else if frame < forward + hold_end + backward {
            let bi = frame - forward - hold_end;
            State {
                active: total - 2 - bi,
                holding: false,
                hold_progress: 0,
                hold_total: 0,
                movement_progress: bi,
                movement_total: backward,
                forward: false,
            }
        } else {
            State {
                active: 0,
                holding: true,
                hold_progress: frame - forward - hold_end - backward,
                hold_total: self.hold_start as i64,
                movement_progress: 0,
                movement_total: 0,
                forward: false,
            }
        }
    }

    /// Trail intensities: `(alpha, brightness)` for the head and five trailing cells.
    fn trail() -> [(f32, f32); 6] {
        let mut t = [(1.0, 1.0); 6];
        for (i, slot) in t.iter_mut().enumerate() {
            *slot = match i {
                0 => (1.0, 1.0),
                1 => (0.9, 1.15),
                _ => (0.65f32.powi(i as i32 - 1), 1.0),
            };
        }
        t
    }

    fn color_index(&self, st: State, ci: i64) -> i64 {
        let trail = Self::trail().len() as i64;
        let dist = if st.forward {
            st.active - ci
        } else {
            ci - st.active
        };
        if st.holding {
            return dist + st.hold_progress;
        }
        if dist > 0 && dist < trail {
            return dist;
        }
        if dist == 0 {
            return 0;
        }
        -1
    }

    /// The cells of one frame as `(glyph, colour)`.
    pub fn cells(&self, frame: usize) -> Vec<(char, Color)> {
        let st = self.state(frame);
        let trail = Self::trail();
        let (r, g, b) = crate::theme::rgb_of(self.color).unwrap_or((255, 255, 255));
        let fade = if st.holding && st.hold_total > 0 {
            let p = (st.hold_progress as f32 / st.hold_total as f32).min(1.0);
            (1.0 - p * (1.0 - self.min_alpha)).max(self.min_alpha)
        } else if !st.holding && st.movement_total > 0 {
            let p = (st.movement_progress as f32 / (st.movement_total - 1).max(1) as f32).min(1.0);
            self.min_alpha + p * (1.0 - self.min_alpha)
        } else {
            1.0
        };
        (0..self.width as i64)
            .map(|ci| {
                let idx = self.color_index(st, ci);
                if idx >= 0 && (idx as usize) < trail.len() {
                    let (alpha, bright) = trail[idx as usize];
                    let lift = |c: u8| ((c as f32 * bright).min(255.0)) as u8;
                    let fg = Color::Rgb(lift(r), lift(g), lift(b));
                    ('■', Theme::alpha_over(fg, self.bg, alpha))
                } else {
                    (
                        '⬝',
                        Theme::alpha_over(self.color, self.bg, self.inactive_factor * fade),
                    )
                }
            })
            .collect()
    }

    /// One span per cell, ready to put in a `Line`.
    pub fn spans(&self, elapsed: Duration) -> Vec<Span<'static>> {
        self.cells(self.frame_at(elapsed))
            .into_iter()
            .map(|(ch, c)| Span::styled(ch.to_string(), Style::new().fg(c)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn glyphs(s: &Scanner, f: usize) -> String {
        s.cells(f).into_iter().map(|(c, _)| c).collect()
    }

    fn scanner() -> Scanner {
        Scanner::prompt(Color::Rgb(92, 156, 245), Color::Rgb(30, 30, 30))
    }

    #[test]
    fn braille_cycles_every_80ms() {
        assert_eq!(braille_frame(Duration::ZERO), "⠋");
        assert_eq!(braille_frame(Duration::from_millis(79)), "⠋");
        assert_eq!(braille_frame(Duration::from_millis(80)), "⠙");
        assert_eq!(braille_frame(Duration::from_millis(800)), "⠋");
    }

    #[test]
    fn scanner_has_opencode_frame_count() {
        assert_eq!(scanner().total_frames(), 8 + 9 + 7 + 30);
    }

    #[test]
    fn head_sweeps_right_with_trail_behind_it() {
        let s = scanner();
        assert_eq!(glyphs(&s, 0), "■⬝⬝⬝⬝⬝⬝⬝");
        assert_eq!(glyphs(&s, 3), "■■■■⬝⬝⬝⬝");
        // Trail is six cells long including the head.
        assert_eq!(glyphs(&s, 7), "⬝⬝■■■■■■");
    }

    #[test]
    fn head_returns_leftward_after_the_end_hold() {
        let s = scanner();
        // forward 8 + hold 9 = frame 17 is the first backward frame, head at width-2.
        let cells = glyphs(&s, 17);
        assert_eq!(cells.chars().nth(6), Some('■'));
        // the cell the head just left is the first trail cell
        assert_eq!(cells.chars().nth(7), Some('■'));
        assert_eq!(cells.chars().next(), Some('⬝'));
    }

    #[test]
    fn head_is_brightest_and_trail_fades() {
        let s = scanner();
        let c = s.cells(7);
        let lum = |x: Color| match x {
            Color::Rgb(r, g, b) => r as u32 + g as u32 + b as u32,
            _ => 0,
        };
        // Head at index 7 and trail going left: luminance should fall away from the head.
        assert!(lum(c[7].1) >= lum(c[5].1));
        assert!(lum(c[5].1) > lum(c[2].1));
    }

    #[test]
    fn every_frame_and_odd_sizes_are_safe() {
        let s = scanner();
        for f in 0..s.total_frames() * 2 {
            assert_eq!(s.cells(f).len(), 8);
        }
        let tiny = Scanner {
            width: 1,
            ..scanner()
        };
        for f in 0..tiny.total_frames() * 2 {
            assert_eq!(tiny.cells(f).len(), 1);
        }
        let zero = Scanner {
            width: 0,
            hold_start: 0,
            hold_end: 0,
            ..scanner()
        };
        assert!(zero.cells(0).is_empty());
        let _ = zero.frame_at(Duration::from_secs(1));
    }

    #[test]
    fn spans_follow_elapsed_time() {
        let s = scanner();
        let a: String = s
            .spans(Duration::ZERO)
            .iter()
            .map(|x| x.content.to_string())
            .collect();
        let b: String = s
            .spans(Duration::from_millis(40 * 3))
            .iter()
            .map(|x| x.content.to_string())
            .collect();
        assert_eq!(a, "■⬝⬝⬝⬝⬝⬝⬝");
        assert_eq!(b, "■■■■⬝⬝⬝⬝");
    }
}
