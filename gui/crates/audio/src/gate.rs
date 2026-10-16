//! Echo gate for duplex calls without an echo canceller. While the far end
//! is playing, microphone frames only pass when they are clearly louder than
//! the echo the room has been measured to return; otherwise the network gets
//! silence, so a server-side voice detector never hears the agent talk over
//! itself. Headphones measure no echo and pass any speech.
use std::collections::VecDeque;

/// Playback level history kept for echo alignment: speaker buffering plus the
/// acoustic path, in 10 ms steps.
const REFERENCE_WINDOW: usize = 30;
/// Reference loudness below which nothing is considered playing.
const PLAYING: f32 = 0.004;
/// Frames the gate stays open after speech passed it (a word's gaps).
const HOLD_FRAMES: u32 = 18;
/// Frames of microphone audio kept to replay a word's onset when it opens.
const PREROLL_FRAMES: usize = 10;
/// Speech must be this much louder than the expected echo.
const MARGIN: f32 = 2.2;
/// And at least this loud in absolute terms while something plays.
const SPEECH_FLOOR: f32 = 0.018;
/// Echo measurements kept: the loudest of the last two seconds of gated
/// frames is the echo to beat, so pauses between words never lower it.
const COUPLING_WINDOW: usize = 100;
/// Measurements needed before they replace the prior.
const COUPLING_MIN: usize = 25;
/// Echo assumed before playback has been measured: laptop speakers.
const COUPLING_PRIOR: f32 = 0.6;

pub struct EchoGate {
    reference: VecDeque<f32>,
    /// Echo returned per unit of playback, learned while the gate is shut.
    ratios: VecDeque<f32>,
    coupling: f32,
    noise_floor: f32,
    hold: u32,
    preroll: VecDeque<Vec<f32>>,
}

/// What to send for one microphone frame.
pub enum GateOutput {
    /// The frame (and, on opening, the held onset before it).
    Pass(Vec<Vec<f32>>),
    /// Silence of the same length.
    Silence,
}

impl Default for EchoGate {
    fn default() -> Self {
        Self {
            reference: VecDeque::with_capacity(REFERENCE_WINDOW),
            ratios: VecDeque::with_capacity(COUPLING_WINDOW),
            coupling: COUPLING_PRIOR,
            noise_floor: 0.002,
            hold: 0,
            preroll: VecDeque::with_capacity(PREROLL_FRAMES),
        }
    }
}

impl EchoGate {
    /// Loudness of 10 ms of audio actually sent to the speaker.
    pub fn reference(&mut self, level: f32) {
        if self.reference.len() == REFERENCE_WINDOW {
            self.reference.pop_front();
        }
        self.reference.push_back(level.max(0.0));
    }

    fn recent_reference(&self) -> f32 {
        self.reference.iter().copied().fold(0.0, f32::max)
    }

    /// Whether the far end is (or was just) audible.
    pub fn playing(&self) -> bool {
        self.recent_reference() >= PLAYING
    }

    /// Gate one microphone frame.
    pub fn process(&mut self, frame: &[f32]) -> GateOutput {
        let level = crate::dsp::level(frame);
        let reference = self.recent_reference();
        if reference < PLAYING {
            // Nothing playing: everything passes and the noise floor settles.
            self.noise_floor = if level < self.noise_floor {
                self.noise_floor * 0.9 + level * 0.1
            } else {
                self.noise_floor * 0.999 + level * 0.001
            };
            self.hold = 0;
            self.preroll.clear();
            return GateOutput::Pass(vec![frame.to_vec()]);
        }
        let expected = self.coupling * reference;
        let threshold = (expected * MARGIN)
            .max(self.noise_floor * 4.0)
            .max(SPEECH_FLOOR);
        let speech = level > threshold;
        if speech || self.hold > 0 {
            let mut out: Vec<Vec<f32>> = if speech && self.hold == 0 {
                self.preroll.drain(..).collect()
            } else {
                Vec::new()
            };
            out.push(frame.to_vec());
            self.hold = if speech { HOLD_FRAMES } else { self.hold - 1 };
            return GateOutput::Pass(out);
        }
        // Gate shut: this is (mostly) echo. Learn how much comes back, never
        // from frames that passed as speech, and bounded so a word spoken
        // just under the threshold cannot ratchet it up.
        let ratio = (level / reference).min(self.coupling * 2.0 + 0.05);
        if self.ratios.len() == COUPLING_WINDOW {
            self.ratios.pop_front();
        }
        self.ratios.push_back(ratio);
        if self.ratios.len() >= COUPLING_MIN {
            self.coupling = self
                .ratios
                .iter()
                .copied()
                .fold(0.0, f32::max)
                .clamp(0.0, 2.0);
        }
        if self.preroll.len() == PREROLL_FRAMES {
            self.preroll.pop_front();
        }
        self.preroll.push_back(frame.to_vec());
        GateOutput::Silence
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(level: f32, n: usize, phase: usize) -> Vec<f32> {
        (0..n)
            .map(|i| ((phase + i) as f32 * 0.07).sin() * level * std::f32::consts::SQRT_2)
            .collect()
    }

    fn passes(out: &GateOutput) -> bool {
        matches!(out, GateOutput::Pass(_))
    }

    #[test]
    fn everything_passes_while_nothing_plays() {
        let mut gate = EchoGate::default();
        for i in 0..20 {
            gate.reference(0.0);
            assert!(passes(&gate.process(&tone(0.001, 480, i * 480))));
        }
    }

    #[test]
    fn speaker_echo_is_held_back_and_louder_speech_barges_in() {
        let mut gate = EchoGate::default();
        // A second of playback whose echo returns at a third of its level.
        for i in 0..50 {
            gate.reference(0.2);
            gate.reference(0.2);
            let out = gate.process(&tone(0.2 * 0.33, 480, i * 480));
            assert!(!passes(&out), "echo frame {i} leaked");
        }
        assert!((gate.coupling - 0.33).abs() < 0.12, "{}", gate.coupling);
        // The user talks over it, well above the echo.
        gate.reference(0.2);
        gate.reference(0.2);
        match gate.process(&tone(0.35, 480, 0)) {
            GateOutput::Pass(frames) => assert!(frames.len() > 1, "onset replayed"),
            GateOutput::Silence => panic!("speech held back"),
        }
        // A short pause inside the sentence keeps the gate open.
        gate.reference(0.2);
        assert!(passes(&gate.process(&tone(0.02, 480, 0))));
    }

    #[test]
    fn headphones_learn_no_echo_and_pass_quiet_speech() {
        let mut gate = EchoGate::default();
        for i in 0..100 {
            gate.reference(0.25);
            gate.reference(0.25);
            gate.process(&tone(0.0005, 480, i * 480));
        }
        assert!(gate.coupling < 0.05, "{}", gate.coupling);
        gate.reference(0.25);
        assert!(passes(&gate.process(&tone(0.04, 480, 0))));
    }

    #[test]
    fn the_echo_tail_after_playback_is_still_gated() {
        let mut gate = EchoGate::default();
        for _ in 0..40 {
            gate.reference(0.2);
            gate.process(&tone(0.06, 480, 0));
        }
        // Playback stopped 100 ms ago; the room still rings.
        for _ in 0..10 {
            gate.reference(0.0);
        }
        assert!(gate.playing());
        assert!(!passes(&gate.process(&tone(0.03, 480, 0))));
        for _ in 0..REFERENCE_WINDOW {
            gate.reference(0.0);
        }
        assert!(!gate.playing());
        assert!(passes(&gate.process(&tone(0.03, 480, 0))));
    }
}
