// OWNER: bottom-pane
//! Paste-burst detection for terminals that deliver a paste as a stream of key events, ported
//! from Codex's `bottom_pane/paste_burst.rs` (spec C.2.5). Pure state machine: the composer
//! feeds it plain characters and applies the decisions. It is only switched on for the real
//! terminal (see `Composer::set_paste_burst`); the headless harness types instantly and would
//! otherwise be read as a paste.

use std::time::Duration;
use std::time::Instant;

// Heuristic thresholds for detecting paste-like input bursts.
// Detect quickly to avoid showing typed prefix before paste is recognized
const PASTE_BURST_MIN_CHARS: u16 = 3;
const PASTE_ENTER_SUPPRESS_WINDOW: Duration = Duration::from_millis(120);

// Maximum delay between consecutive chars to be considered part of a paste burst.
const PASTE_BURST_CHAR_INTERVAL: Duration = Duration::from_millis(8);

// Idle timeout before flushing buffered paste content.
// Slower paste bursts have been observed in Windows environments.
#[cfg(not(windows))]
const PASTE_BURST_ACTIVE_IDLE_TIMEOUT: Duration = Duration::from_millis(8);
#[cfg(windows)]
const PASTE_BURST_ACTIVE_IDLE_TIMEOUT: Duration = Duration::from_millis(60);

#[derive(Default)]
pub struct PasteBurst {
    last_plain_char_time: Option<Instant>,
    consecutive_plain_char_burst: u16,
    burst_window_until: Option<Instant>,
    buffer: String,
    active: bool,
    // Hold first fast char briefly to avoid rendering flicker
    pending_first_char: Option<(char, Instant)>,
}

pub enum CharDecision {
    BeginBuffer { retro_chars: u16 },
    BufferAppend,
    RetainFirstChar,
    BeginBufferFromPending,
}

pub struct RetroGrab {
    pub start_byte: usize,
    pub grabbed: String,
}

pub enum FlushResult {
    Paste(String),
    Typed(char),
    None,
}

impl PasteBurst {
    pub fn recommended_flush_delay() -> Duration {
        PASTE_BURST_CHAR_INTERVAL + Duration::from_millis(1)
    }

    pub fn on_plain_char(&mut self, ch: char, now: Instant) -> CharDecision {
        self.note_plain_char(now);

        if self.active {
            self.burst_window_until = Some(now + PASTE_ENTER_SUPPRESS_WINDOW);
            return CharDecision::BufferAppend;
        }

        // If we already held a first char and receive a second fast char,
        // start buffering without retro-grabbing (we never rendered the first).
        if let Some((held, held_at)) = self.pending_first_char {
            if now.duration_since(held_at) <= PASTE_BURST_CHAR_INTERVAL {
                self.active = true;
                let _ = self.pending_first_char.take();
                self.buffer.push(held);
                self.burst_window_until = Some(now + PASTE_ENTER_SUPPRESS_WINDOW);
                return CharDecision::BeginBufferFromPending;
            }
        }

        if self.consecutive_plain_char_burst >= PASTE_BURST_MIN_CHARS {
            return CharDecision::BeginBuffer {
                retro_chars: self.consecutive_plain_char_burst.saturating_sub(1),
            };
        }

        // Save the first fast char very briefly to see if a burst follows.
        self.pending_first_char = Some((ch, now));
        CharDecision::RetainFirstChar
    }

    pub fn on_plain_char_no_hold(&mut self, now: Instant) -> Option<CharDecision> {
        self.note_plain_char(now);

        if self.active {
            self.burst_window_until = Some(now + PASTE_ENTER_SUPPRESS_WINDOW);
            return Some(CharDecision::BufferAppend);
        }

        if self.consecutive_plain_char_burst >= PASTE_BURST_MIN_CHARS {
            return Some(CharDecision::BeginBuffer {
                retro_chars: self.consecutive_plain_char_burst.saturating_sub(1),
            });
        }

        None
    }

    fn note_plain_char(&mut self, now: Instant) {
        match self.last_plain_char_time {
            Some(prev) if now.duration_since(prev) <= PASTE_BURST_CHAR_INTERVAL => {
                self.consecutive_plain_char_burst =
                    self.consecutive_plain_char_burst.saturating_add(1)
            }
            _ => self.consecutive_plain_char_burst = 1,
        }
        self.last_plain_char_time = Some(now);
    }

    pub fn flush_if_due(&mut self, now: Instant) -> FlushResult {
        let timeout = if self.is_active_internal() {
            PASTE_BURST_ACTIVE_IDLE_TIMEOUT
        } else {
            PASTE_BURST_CHAR_INTERVAL
        };
        let timed_out = self
            .last_plain_char_time
            .is_some_and(|t| now.duration_since(t) > timeout);
        if timed_out && self.is_active_internal() {
            self.active = false;
            let out = std::mem::take(&mut self.buffer);
            FlushResult::Paste(out)
        } else if timed_out {
            // If we were saving a single fast char and no burst followed,
            // flush it as normal typed input.
            if let Some((ch, _at)) = self.pending_first_char.take() {
                FlushResult::Typed(ch)
            } else {
                FlushResult::None
            }
        } else {
            FlushResult::None
        }
    }

    pub fn append_newline_if_active(&mut self, now: Instant) -> bool {
        if self.is_active() {
            self.buffer.push('\n');
            self.burst_window_until = Some(now + PASTE_ENTER_SUPPRESS_WINDOW);
            true
        } else {
            false
        }
    }

    pub fn newline_should_insert_instead_of_submit(&self, now: Instant) -> bool {
        let in_burst_window = self.burst_window_until.is_some_and(|until| now <= until);
        self.is_active() || in_burst_window
    }

    pub fn direct_insert_newline_should_insert(&self, now: Instant) -> bool {
        self.newline_should_insert_instead_of_submit(now)
            || self
                .last_plain_char_time
                .is_some_and(|t| now.duration_since(t) <= PASTE_BURST_CHAR_INTERVAL)
    }

    pub fn extend_window(&mut self, now: Instant) {
        self.burst_window_until = Some(now + PASTE_ENTER_SUPPRESS_WINDOW);
    }

    pub fn begin_with_retro_grabbed(&mut self, grabbed: String, now: Instant) {
        if !grabbed.is_empty() {
            self.buffer.push_str(&grabbed);
        }
        self.active = true;
        self.burst_window_until = Some(now + PASTE_ENTER_SUPPRESS_WINDOW);
    }

    pub fn append_char_to_buffer(&mut self, ch: char, now: Instant) {
        self.buffer.push(ch);
        self.burst_window_until = Some(now + PASTE_ENTER_SUPPRESS_WINDOW);
    }

    pub fn try_append_char_if_active(&mut self, ch: char, now: Instant) -> bool {
        if self.active || !self.buffer.is_empty() {
            self.append_char_to_buffer(ch, now);
            true
        } else {
            false
        }
    }

    pub fn decide_begin_buffer(
        &mut self,
        now: Instant,
        before: &str,
        retro_chars: usize,
    ) -> Option<RetroGrab> {
        let start_byte = retro_start_index(before, retro_chars);
        let grabbed = before[start_byte..].to_string();
        let looks_pastey =
            grabbed.chars().any(char::is_whitespace) || grabbed.chars().count() >= 16;
        if looks_pastey {
            // Note: caller is responsible for removing this slice from UI text.
            self.begin_with_retro_grabbed(grabbed.clone(), now);
            Some(RetroGrab {
                start_byte,
                grabbed,
            })
        } else {
            None
        }
    }

    pub fn flush_before_modified_input(&mut self) -> Option<String> {
        if !self.is_active() {
            return None;
        }
        self.active = false;
        let mut out = std::mem::take(&mut self.buffer);
        if let Some((ch, _at)) = self.pending_first_char.take() {
            out.push(ch);
        }
        Some(out)
    }

    pub fn clear_window_after_non_char(&mut self) {
        self.consecutive_plain_char_burst = 0;
        self.last_plain_char_time = None;
        self.burst_window_until = None;
        self.active = false;
        self.pending_first_char = None;
    }

    pub fn is_active(&self) -> bool {
        self.is_active_internal() || self.pending_first_char.is_some()
    }

    fn is_active_internal(&self) -> bool {
        self.active || !self.buffer.is_empty()
    }

    pub fn clear_after_explicit_paste(&mut self) {
        self.last_plain_char_time = None;
        self.consecutive_plain_char_burst = 0;
        self.burst_window_until = None;
        self.active = false;
        self.buffer.clear();
        self.pending_first_char = None;
    }
}

pub fn retro_start_index(before: &str, retro_chars: usize) -> usize {
    if retro_chars == 0 {
        return before.len();
    }
    before
        .char_indices()
        .rev()
        .nth(retro_chars.saturating_sub(1))
        .map(|(idx, _)| idx)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_ascii_char_is_held_then_flushes_as_typed() {
        let mut b = PasteBurst::default();
        let t0 = Instant::now();
        assert!(matches!(
            b.on_plain_char('a', t0),
            CharDecision::RetainFirstChar
        ));
        let t1 = t0 + PasteBurst::recommended_flush_delay() + Duration::from_millis(1);
        assert!(matches!(b.flush_if_due(t1), FlushResult::Typed('a')));
        assert!(!b.is_active());
    }

    #[test]
    fn two_fast_chars_buffer_and_flush_as_a_paste() {
        let mut b = PasteBurst::default();
        let t0 = Instant::now();
        assert!(matches!(
            b.on_plain_char('a', t0),
            CharDecision::RetainFirstChar
        ));
        let t1 = t0 + Duration::from_millis(1);
        assert!(matches!(
            b.on_plain_char('b', t1),
            CharDecision::BeginBufferFromPending
        ));
        b.append_char_to_buffer('b', t1);
        let t2 = t1 + Duration::from_millis(30);
        match b.flush_if_due(t2) {
            FlushResult::Paste(s) => assert_eq!(s, "ab"),
            _ => panic!("expected a paste"),
        }
    }

    #[test]
    fn enter_inside_the_window_is_a_newline() {
        let mut b = PasteBurst::default();
        let t0 = Instant::now();
        b.on_plain_char('a', t0);
        b.on_plain_char('b', t0 + Duration::from_millis(1));
        b.append_char_to_buffer('b', t0 + Duration::from_millis(1));
        assert!(matches!(
            b.flush_if_due(t0 + Duration::from_millis(20)),
            FlushResult::Paste(_)
        ));
        assert!(b.newline_should_insert_instead_of_submit(t0 + Duration::from_millis(50)));
        assert!(!b.newline_should_insert_instead_of_submit(t0 + Duration::from_millis(500)));
    }

    #[test]
    fn slow_typing_is_never_a_paste() {
        let mut b = PasteBurst::default();
        let mut t = Instant::now();
        let mut typed = String::new();
        for ch in "hello".chars() {
            assert!(matches!(
                b.on_plain_char(ch, t),
                CharDecision::RetainFirstChar
            ));
            t += Duration::from_millis(40);
            if let FlushResult::Typed(c) = b.flush_if_due(t) {
                typed.push(c);
            }
        }
        assert_eq!(typed, "hello");
    }

    #[test]
    fn retro_grab_needs_whitespace_or_length() {
        let mut b = PasteBurst::default();
        let now = Instant::now();
        assert!(b.decide_begin_buffer(now, "abc", 3).is_none());
        let g = b.decide_begin_buffer(now, "x ab", 3).unwrap();
        assert_eq!(g.grabbed, " ab");
        assert_eq!(retro_start_index("héllo", 2), 4);
    }
}
