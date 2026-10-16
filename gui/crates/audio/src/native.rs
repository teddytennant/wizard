//! CPAL streams: callbacks touch only preallocated lock-free rings and atomics.
use crate::{AudioError, dsp::Resampler};
use cpal::{
    FromSample, Sample, SampleFormat, SizedSample, Stream, StreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use crossbeam_queue::ArrayQueue;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

const MAX_RING_SAMPLES: usize = 38_400;
fn ring_samples(rate: u32) -> usize {
    (rate as usize / 5).min(MAX_RING_SAMPLES)
}
/// Capture and echo-reference rings hold half a second: a server that
/// delivers in bursts, or a late poll, must not overflow them.
fn burst_ring_samples(rate: u32) -> usize {
    (rate as usize / 2).min(MAX_RING_SAMPLES * 3)
}
/// A short device buffer when the device takes one. Without it a sound
/// server may pick its own (PulseAudio's capture default is seconds), which
/// both delays the audio and arrives in bursts larger than the rings.
fn small_buffer(config: &cpal::SupportedStreamConfig, ms: u32) -> cpal::BufferSize {
    let frames = config.sample_rate() * ms / 1000;
    match config.buffer_size() {
        cpal::SupportedBufferSize::Range { min, max } if (*min..=*max).contains(&frames) => {
            cpal::BufferSize::Fixed(frames)
        }
        _ => cpal::BufferSize::Default,
    }
}
#[derive(Clone, Copy)]
struct OutputSample {
    generation: u64,
    value: f32,
}
/// Open returns paused streams. Caller starts after explicit user action and
/// eligibility; merely constructing preferences must never open a microphone.
pub struct AudioIo {
    input: Stream,
    output: Stream,
    pub capture: Arc<ArrayQueue<f32>>,
    pub reference: Arc<ArrayQueue<f32>>,
    playout: Arc<ArrayQueue<OutputSample>>,
    /// Output gain as f32 bits, applied in the callback so a duck is heard
    /// at once rather than after the samples already queued.
    gain: Arc<AtomicU32>,
    pub failed: Arc<AtomicBool>,
    pub muted: Arc<AtomicBool>,
    generation: Arc<AtomicU64>,
    pub input_rate: u32,
    pub output_rate: u32,
    output_resampler: Resampler,
    scratch: Vec<f32>,
}
impl AudioIo {
    pub fn open(generation: u64) -> Result<Self, AudioError> {
        let host = cpal::default_host();
        let input_device = host.default_input_device().ok_or(AudioError::Device)?;
        let output_device = host.default_output_device().ok_or(AudioError::Device)?;
        Self::open_devices(&input_device, &output_device, generation)
    }

    pub fn open_devices(
        input_device: &cpal::Device,
        output_device: &cpal::Device,
        generation: u64,
    ) -> Result<Self, AudioError> {
        let input_config = input_device
            .default_input_config()
            .map_err(|_| AudioError::Device)?;
        let output_config = output_device
            .default_output_config()
            .map_err(|_| AudioError::Device)?;
        let input_rate = input_config.sample_rate();
        let output_rate = output_config.sample_rate();
        let capture = Arc::new(ArrayQueue::new(burst_ring_samples(input_rate)));
        let reference = Arc::new(ArrayQueue::new(burst_ring_samples(output_rate)));
        let playout = Arc::new(ArrayQueue::new(ring_samples(output_rate)));
        let failed = Arc::new(AtomicBool::new(false));
        let muted = Arc::new(AtomicBool::new(false));
        let gain = Arc::new(AtomicU32::new(1.0f32.to_bits()));
        let generation = Arc::new(AtomicU64::new(generation));
        let ic: StreamConfig = input_config.clone().into();
        let oc: StreamConfig = output_config.clone().into();
        let ic_small = StreamConfig {
            buffer_size: small_buffer(&input_config, 10),
            ..ic
        };
        let oc_small = StreamConfig {
            buffer_size: small_buffer(&output_config, 20),
            ..oc
        };
        if ic.channels == 0 || oc.channels == 0 || ic.channels > 8 || oc.channels > 8 {
            return Err(AudioError::Device);
        }
        let output_resampler = Resampler::new(24_000, output_rate).ok_or(AudioError::Device)?;
        if Resampler::new(input_rate, 48_000).is_none() {
            return Err(AudioError::Device);
        }
        macro_rules! input {
            ($t:ty, $config:expr) => {
                input_stream::<$t>(
                    input_device,
                    $config,
                    capture.clone(),
                    failed.clone(),
                    muted.clone(),
                )
            };
        }
        macro_rules! output {
            ($t:ty, $config:expr) => {
                output_stream::<$t>(
                    output_device,
                    $config,
                    playout.clone(),
                    reference.clone(),
                    failed.clone(),
                    generation.clone(),
                    gain.clone(),
                )
            };
        }
        let build_input = |config: StreamConfig| match input_config.sample_format() {
            SampleFormat::F32 => input!(f32, config),
            SampleFormat::F64 => input!(f64, config),
            SampleFormat::I16 => input!(i16, config),
            SampleFormat::U16 => input!(u16, config),
            SampleFormat::I32 => input!(i32, config),
            SampleFormat::U32 => input!(u32, config),
            SampleFormat::I8 => input!(i8, config),
            SampleFormat::U8 => input!(u8, config),
            _ => Err(AudioError::Device),
        };
        let build_output = |config: StreamConfig| match output_config.sample_format() {
            SampleFormat::F32 => output!(f32, config),
            SampleFormat::F64 => output!(f64, config),
            SampleFormat::I16 => output!(i16, config),
            SampleFormat::U16 => output!(u16, config),
            SampleFormat::I32 => output!(i32, config),
            SampleFormat::U32 => output!(u32, config),
            SampleFormat::I8 => output!(i8, config),
            SampleFormat::U8 => output!(u8, config),
            _ => Err(AudioError::Device),
        };
        // A device that refuses the short buffer still opens with its own.
        let input = build_input(ic_small).or_else(|_| build_input(ic))?;
        let output = build_output(oc_small).or_else(|_| build_output(oc))?;
        Ok(Self {
            input,
            output,
            capture,
            reference,
            playout,
            gain,
            failed,
            muted,
            generation,
            input_rate,
            output_rate,
            output_resampler,
            scratch: Vec::with_capacity(MAX_RING_SAMPLES * 3),
        })
    }
    pub fn start(&self) -> Result<(), AudioError> {
        self.output.play().map_err(|_| AudioError::Device)?;
        if self.input.play().is_err() {
            let _ = self.output.pause();
            return Err(AudioError::Device);
        }
        Ok(())
    }
    pub fn enqueue(&mut self, frame: &zeron_proto::voice::VoiceFrame) -> Result<(), AudioError> {
        if frame.generation != self.generation.load(Ordering::Acquire) {
            return Err(AudioError::Frame);
        }
        let samples = crate::decode(frame)?;
        self.scratch.clear();
        for sample in samples {
            self.output_resampler.push(sample, &mut self.scratch);
        }
        if self.scratch.len() > self.playout.capacity() - self.playout.len() {
            return Err(AudioError::Overflow);
        }
        for value in &self.scratch {
            self.playout
                .push(OutputSample {
                    generation: frame.generation,
                    value: *value,
                })
                .map_err(|_| AudioError::Overflow)?;
        }
        Ok(())
    }
    /// Queue 24 kHz mono samples for playout in the current generation.
    /// Nothing is queued when they do not all fit; see [`Self::playout_room`].
    pub fn enqueue_samples(&mut self, samples: &[f32]) -> Result<(), AudioError> {
        let generation = self.generation.load(Ordering::Acquire);
        self.scratch.clear();
        for sample in samples {
            self.output_resampler.push(*sample, &mut self.scratch);
        }
        if self.scratch.len() > self.playout.capacity() - self.playout.len() {
            return Err(AudioError::Overflow);
        }
        for value in &self.scratch {
            self.playout
                .push(OutputSample {
                    generation,
                    value: *value,
                })
                .map_err(|_| AudioError::Overflow)?;
        }
        Ok(())
    }

    /// How many more 24 kHz samples [`Self::enqueue_samples`] accepts now,
    /// leaving a little slack for resampler rounding.
    pub fn playout_room(&self) -> usize {
        let free = self.playout.capacity() - self.playout.len();
        (free as u64 * 24_000 / self.output_rate as u64).saturating_sub(4) as usize
    }

    /// Output samples still waiting for the device.
    pub fn playout_queued(&self) -> usize {
        self.playout.len()
    }

    pub fn set_gain(&self, gain: f32) {
        self.gain
            .store(gain.clamp(0.0, 1.0).to_bits(), Ordering::Release);
    }

    pub fn mute(&self, value: bool) {
        self.muted.store(value, Ordering::Release);
        while self.capture.pop().is_some() {}
    }
    pub fn flush(&mut self, next_generation: u64) {
        self.generation.store(next_generation, Ordering::Release);
        while self.playout.pop().is_some() {}
        while self.reference.pop().is_some() {}
        while self.capture.pop().is_some() {}
        self.output_resampler = Resampler::new(24_000, self.output_rate).unwrap();
    }
}
impl Drop for AudioIo {
    fn drop(&mut self) {
        self.muted.store(true, Ordering::Release);
        let _ = self.input.pause();
        let _ = self.output.pause();
        self.generation.fetch_add(1, Ordering::AcqRel);
        while self.playout.pop().is_some() {}
    }
}
fn input_stream<T>(
    device: &cpal::Device,
    config: StreamConfig,
    ring: Arc<ArrayQueue<f32>>,
    failed: Arc<AtomicBool>,
    muted: Arc<AtomicBool>,
) -> Result<Stream, AudioError>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let channels = config.channels as usize;
    let errors = failed.clone();
    device
        .build_input_stream(
            config,
            move |samples: &[T], _| {
                if muted.load(Ordering::Acquire) {
                    return;
                }
                for frame in samples.chunks_exact(channels) {
                    let mono =
                        frame.iter().map(|s| f32::from_sample(*s)).sum::<f32>() / channels as f32;
                    if ring.push(mono).is_err() {
                        failed.store(true, Ordering::Release);
                        break;
                    }
                }
            },
            move |_| {
                errors.store(true, Ordering::Release);
            },
            Some(std::time::Duration::from_secs(2)),
        )
        .map_err(|_| AudioError::Device)
}
fn output_stream<T>(
    device: &cpal::Device,
    config: StreamConfig,
    ring: Arc<ArrayQueue<OutputSample>>,
    reference: Arc<ArrayQueue<f32>>,
    failed: Arc<AtomicBool>,
    generation: Arc<AtomicU64>,
    gain: Arc<AtomicU32>,
) -> Result<Stream, AudioError>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = config.channels as usize;
    let errors = failed.clone();
    device
        .build_output_stream(
            config,
            move |samples: &mut [T], _| {
                let gain = f32::from_bits(gain.load(Ordering::Acquire));
                for frame in samples.chunks_exact_mut(channels) {
                    let value = ring
                        .pop()
                        .filter(|s| s.generation == generation.load(Ordering::Acquire))
                        .map_or(0.0, |s| s.value * gain);
                    for sample in frame {
                        *sample = T::from_sample(value);
                    }
                    // Exactly what was sent to the device, including underrun silence.
                    if reference.push(value).is_err() {
                        failed.store(true, Ordering::Release);
                    }
                }
            },
            move |_| {
                errors.store(true, Ordering::Release);
            },
            Some(std::time::Duration::from_secs(2)),
        )
        .map_err(|_| AudioError::Device)
}
