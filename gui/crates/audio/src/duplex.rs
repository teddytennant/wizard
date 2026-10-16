//! A call's two-way audio on a thread of its own: 20 ms frames of 24 kHz mono
//! PCM16 out of the microphone, and 24 kHz audio from the far end into the
//! speaker. Far-end audio may arrive far faster than real time; it waits in a
//! jitter buffer and feeds the device ring as it drains. A flush (barge-in)
//! drops everything queued at once. While playback is audible the echo gate
//! decides what the microphone may send, and speech that gets through ducks
//! the playback immediately, before any server has decided to interrupt.
use crate::{
    AudioError,
    dsp::{Resampler, level},
    gate::{EchoGate, GateOutput},
    native::AudioIo,
};
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Network frame: 20 ms at 24 kHz.
pub const FRAME_SAMPLES: usize = 480;
const RATE: u32 = 24_000;
/// Audio held before a reply starts playing, so its first words never
/// stutter on a late packet.
const PREBUFFER: usize = RATE as usize * 80 / 1000;
const PREBUFFER_WAIT: Duration = Duration::from_millis(60);
/// Playback gain while speech is heard over it, until the far end stops.
const DUCK_GAIN: f32 = 0.25;
const DUCK_HOLD: Duration = Duration::from_millis(900);
const TICK: Duration = Duration::from_millis(5);
/// Frames dropped right after the microphone opens: digital microphones pop
/// as they power up, which would read as speech.
const WARMUP_FRAMES: usize = 5;

enum Command {
    Play(Vec<f32>),
    Flush,
    Mute(bool),
    Stop,
}

#[derive(Default)]
struct Shared {
    microphone: AtomicU32,
    speaker: AtomicU32,
    playing: AtomicBool,
    failed: AtomicBool,
}

pub struct Duplex {
    commands: mpsc::Sender<Command>,
    shared: Arc<Shared>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Duplex {
    /// Open the microphone (`input`: a `cpal::DeviceId` string, `None` for
    /// [`crate::devices::input_device`]'s choice) and the default speaker,
    /// and start both. Blocks until the devices are open. `on_frame` runs on
    /// the audio thread with each 20 ms of PCM16LE to send; frames the echo
    /// gate holds back arrive as silence so the far end's clock keeps running.
    pub fn start(
        input: Option<String>,
        on_frame: impl FnMut(Vec<u8>) + Send + 'static,
    ) -> Result<Self, AudioError> {
        let (commands, receiver) = mpsc::channel();
        let shared = Arc::new(Shared::default());
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread_shared = shared.clone();
        let thread = std::thread::Builder::new()
            .name("voice-audio".into())
            .spawn(move || {
                let io = (|| {
                    let host = cpal::default_host();
                    let microphone = crate::devices::input_device(&host, input.as_deref())
                        .ok_or(AudioError::Device)?;
                    let speaker = cpal::traits::HostTrait::default_output_device(&host)
                        .ok_or(AudioError::Device)?;
                    let io = AudioIo::open_devices(&microphone, &speaker, 1)?;
                    io.start()?;
                    Ok(io)
                })();
                match io {
                    Ok(io) => {
                        let _ = ready_tx.send(Ok(()));
                        run(io, receiver, thread_shared, on_frame);
                    }
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                    }
                }
            })
            .map_err(|_| AudioError::Device)?;
        ready_rx.recv().map_err(|_| AudioError::Device)??;
        Ok(Self {
            commands,
            shared,
            thread: Some(thread),
        })
    }

    /// Queue far-end audio (24 kHz mono PCM16LE).
    pub fn play(&self, pcm: &[u8]) {
        let samples = pcm
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0)
            .collect();
        let _ = self.commands.send(Command::Play(samples));
    }

    /// Drop all queued far-end audio now (the user interrupted).
    pub fn flush(&self) {
        let _ = self.commands.send(Command::Flush);
    }

    pub fn set_muted(&self, muted: bool) {
        let _ = self.commands.send(Command::Mute(muted));
    }

    /// Microphone and speaker loudness, 0…1.
    pub fn levels(&self) -> (f32, f32) {
        (
            f32::from_bits(self.shared.microphone.load(Ordering::Relaxed)),
            f32::from_bits(self.shared.speaker.load(Ordering::Relaxed)),
        )
    }

    /// Far-end audio is queued or still sounding.
    pub fn playing(&self) -> bool {
        self.shared.playing.load(Ordering::Relaxed)
    }

    /// A device stopped or disappeared.
    pub fn failed(&self) -> bool {
        self.shared.failed.load(Ordering::Relaxed)
    }
}

impl Drop for Duplex {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// One-pole DC blocker: some built-in microphones sit far off zero, which
/// would read as constant loudness.
#[derive(Default)]
struct DcBlock {
    primed: bool,
    previous_in: f32,
    previous_out: f32,
}
impl DcBlock {
    fn process(&mut self, sample: f32) -> f32 {
        // Start from the first sample, so the offset never reads as a click.
        if !self.primed {
            self.primed = true;
            self.previous_in = sample;
        }
        let out = sample - self.previous_in + 0.995 * self.previous_out;
        self.previous_in = sample;
        self.previous_out = out;
        out
    }
}

fn pcm16(samples: &[f32]) -> Vec<u8> {
    samples
        .iter()
        .flat_map(|s| {
            let value = if s.is_finite() {
                (s.clamp(-1.0, 1.0) * 32767.0) as i16
            } else {
                0
            };
            value.to_le_bytes()
        })
        .collect()
}

fn run(
    mut io: AudioIo,
    commands: mpsc::Receiver<Command>,
    shared: Arc<Shared>,
    mut on_frame: impl FnMut(Vec<u8>),
) {
    let Some(mut to_network) = Resampler::new(io.input_rate, RATE) else {
        shared.failed.store(true, Ordering::Relaxed);
        return;
    };
    let reference_chunk = (io.output_rate / 100).max(1) as usize;
    let mut generation = 1;
    let mut dc = DcBlock::default();
    let mut gate = EchoGate::default();
    let mut queue: VecDeque<f32> = VecDeque::new();
    let mut microphone: Vec<f32> = Vec::with_capacity(FRAME_SAMPLES * 4);
    let mut resampled = Vec::with_capacity(64);
    let mut reference: Vec<f32> = Vec::with_capacity(reference_chunk);
    let mut chunk: Vec<f32> = Vec::new();
    let mut muted = false;
    let mut waiting_since: Option<Instant> = None;
    let mut ducked_until: Option<Instant> = None;
    let mut speaker_level = 0.0f32;
    let mut warmup = WARMUP_FRAMES;
    loop {
        loop {
            match commands.try_recv() {
                Ok(Command::Play(samples)) => {
                    if queue.is_empty() && io.playout_queued() == 0 {
                        waiting_since = Some(Instant::now());
                    }
                    queue.extend(samples);
                }
                Ok(Command::Flush) => {
                    queue.clear();
                    generation += 1;
                    io.flush(generation);
                    io.set_gain(1.0);
                    ducked_until = None;
                    waiting_since = None;
                }
                Ok(Command::Mute(value)) => {
                    muted = value;
                    io.mute(value);
                    microphone.clear();
                }
                Ok(Command::Stop) | Err(mpsc::TryRecvError::Disconnected) => return,
                Err(mpsc::TryRecvError::Empty) => break,
            }
        }
        if io.failed.load(Ordering::Acquire) {
            shared.failed.store(true, Ordering::Relaxed);
            return;
        }

        // Far end -> speaker, once a reply has enough buffered to start.
        let ready = match waiting_since {
            Some(since) => queue.len() >= PREBUFFER || since.elapsed() >= PREBUFFER_WAIT,
            None => true,
        };
        if ready {
            waiting_since = None;
            let take = io.playout_room().min(queue.len()).min(RATE as usize / 10);
            if take > 0 {
                chunk.clear();
                chunk.extend(queue.drain(..take));
                if io.enqueue_samples(&chunk).is_err() {
                    // Rounding left no room: put it back for the next tick.
                    for sample in chunk.drain(..).rev() {
                        queue.push_front(sample);
                    }
                }
            }
        }

        // What the speaker actually played: echo reference and level.
        while let Some(sample) = io.reference.pop() {
            reference.push(sample);
            if reference.len() >= reference_chunk {
                let played = level(&reference);
                gate.reference(played);
                speaker_level = played.max(speaker_level * 0.85);
                reference.clear();
            }
        }
        shared
            .speaker
            .store(speaker_level.to_bits(), Ordering::Relaxed);
        shared.playing.store(
            !queue.is_empty() || io.playout_queued() > 0 || gate.playing(),
            Ordering::Relaxed,
        );

        // Microphone -> network.
        while let Some(sample) = io.capture.pop() {
            resampled.clear();
            to_network.push(dc.process(sample), &mut resampled);
            microphone.extend_from_slice(&resampled);
        }
        while microphone.len() >= FRAME_SAMPLES {
            let frame: Vec<f32> = microphone.drain(..FRAME_SAMPLES).collect();
            if warmup > 0 {
                warmup -= 1;
                continue;
            }
            let heard = level(&frame);
            shared.microphone.store(
                if muted { 0.0f32 } else { heard }.to_bits(),
                Ordering::Relaxed,
            );
            if muted {
                continue;
            }
            let playing = gate.playing();
            match gate.process(&frame) {
                GateOutput::Pass(frames) => {
                    if playing {
                        io.set_gain(DUCK_GAIN);
                        ducked_until = Some(Instant::now() + DUCK_HOLD);
                    }
                    for frame in frames {
                        on_frame(pcm16(&frame));
                    }
                }
                GateOutput::Silence => on_frame(vec![0; FRAME_SAMPLES * 2]),
            }
        }
        if ducked_until.is_some_and(|until| Instant::now() >= until) {
            ducked_until = None;
            io.set_gain(1.0);
        }
        std::thread::sleep(TICK);
    }
}
