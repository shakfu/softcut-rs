//! Audio side of the demo: cpal streams around a `softcut::rt` processor.
//!
//! The UI controls the engine through the `rt::Handle`. `Meters` carries what
//! `rt` does not: the input source, the input level and a waveform overview.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU32, AtomicUsize, Ordering::Relaxed};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use rtrb::{Consumer, RingBuffer};
use softcut::rt::{self, Handle, Processor};
use softcut::{Engine, EngineConfig};

pub const VOICES: usize = 4;
pub const WAVE_BINS: usize = 600;
pub const BUFFER_FRAMES: usize = 1 << 21;
const BLOCK: usize = 64;
const MAX_FRAMES: usize = 4096;
/// Buffer samples rescanned for the waveform per callback.
const SCAN_BUDGET: usize = 32768;
/// Input frames queued beyond this are dropped, to bound mic latency.
const MAX_INPUT_BACKLOG: usize = 2048;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Source {
    Mic,
    /// Mic stream paused; the engine gets zeros.
    Off,
}

impl Source {
    fn from_u8(x: u8) -> Self {
        if x == Source::Mic as u8 {
            Source::Mic
        } else {
            Source::Off
        }
    }
}

/// Shared between the UI and the audio thread. f32 values are stored as bits.
pub struct Meters {
    /// Written by the UI.
    source: AtomicU8,
    /// Absolute peak of buffer 0 per bin.
    peaks: [AtomicU32; WAVE_BINS],
    /// Highest input level since the UI last took it.
    input_peak: AtomicU32,
    /// Frames from the buffer start that the waveform bins span. Written by the UI.
    view_frames: AtomicUsize,
}

impl Meters {
    pub fn set_view_frames(&self, frames: usize) {
        self.view_frames.store(frames, Relaxed);
    }

    pub fn peak(&self, bin: usize) -> f32 {
        f32::from_bits(self.peaks[bin].load(Relaxed))
    }

    /// Highest input level since the last call; resets it.
    pub fn take_input_peak(&self) -> f32 {
        f32::from_bits(self.input_peak.swap(0, Relaxed))
    }
}

pub struct Audio {
    pub handle: Handle,
    pub meters: Arc<Meters>,
    pub sample_rate: f32,
    pub buffer_seconds: f32,
    pub output_name: String,
    /// Why the microphone is unavailable, if it is.
    pub input_error: Option<String>,
    /// Paused unless the source is `Mic`, so the mic is only in use when chosen.
    input_stream: Option<cpal::Stream>,
    _output_stream: cpal::Stream,
}

impl Audio {
    /// Switch the input, starting or pausing the mic stream to match.
    pub fn set_source(&mut self, s: Source) -> Result<(), String> {
        if let Some(stream) = &self.input_stream {
            match s {
                Source::Mic => stream.play(),
                Source::Off => stream.pause(),
            }
            .map_err(|e| e.to_string())?;
        } else if s == Source::Mic {
            return Err(self.input_error.clone().unwrap_or_default());
        }
        self.meters.source.store(s as u8, Relaxed);
        Ok(())
    }
}

/// Opens the default devices and starts the engine. `setup` configures the
/// engine before the audio thread takes it.
pub fn start(setup: impl FnOnce(&mut Engine)) -> Result<Audio, String> {
    let host = cpal::default_host();
    let out_dev = host.default_output_device().ok_or("no output device")?;
    let out_cfg = out_dev.default_output_config().map_err(|e| e.to_string())?;
    if out_cfg.sample_format() != cpal::SampleFormat::F32 {
        return Err(format!(
            "output sample format {:?} unsupported; need f32",
            out_cfg.sample_format()
        ));
    }
    let sample_rate = out_cfg.sample_rate() as f32;
    let out_channels = out_cfg.channels() as usize;
    let output_name = out_dev
        .description()
        .map(|d| d.name().to_string())
        .unwrap_or_default();

    let mut engine = Engine::new(EngineConfig {
        sample_rate,
        voices: VOICES,
        buffers: 1,
        buffer_frames: BUFFER_FRAMES,
        block_size: BLOCK,
        out_channels: 2,
    });
    setup(&mut engine);

    let (handle, processor) = rt::split(engine, 1024);

    let (input_stream, input_rx, input_error) = match open_input(&host, out_cfg.sample_rate()) {
        Ok((stream, rx)) => (Some(stream), Some(rx), None),
        Err(e) => (None, None, Some(e)),
    };
    let meters = Arc::new(Meters {
        source: AtomicU8::new(Source::Off as u8),
        peaks: std::array::from_fn(|_| AtomicU32::new(0)),
        input_peak: AtomicU32::new(0),
        view_frames: AtomicUsize::new(BUFFER_FRAMES),
    });

    let mut state = AudioState {
        processor,
        input: input_rx,
        meters: meters.clone(),
        mono_in: vec![0.0; MAX_FRAMES],
        stereo_out: vec![0.0; MAX_FRAMES * 2],
        scan_bin: 0,
        scan_view: 0,
    };
    let stream = out_dev
        .build_output_stream::<f32, _, _>(
            out_cfg.config(),
            move |data, _| state.render(data, out_channels),
            |e| eprintln!("output stream error: {e}"),
            None,
        )
        .map_err(|e| e.to_string())?;
    stream.play().map_err(|e| e.to_string())?;

    Ok(Audio {
        handle,
        meters,
        sample_rate,
        buffer_seconds: BUFFER_FRAMES as f32 / sample_rate,
        output_name,
        input_error,
        input_stream,
        _output_stream: stream,
    })
}

/// Opens the default input, paused, at the output's sample rate, forwarding
/// its first channel through a ring. softcut does no rate conversion on its input.
fn open_input(
    host: &cpal::Host,
    rate: cpal::SampleRate,
) -> Result<(cpal::Stream, Consumer<f32>), String> {
    let dev = host.default_input_device().ok_or("no input device")?;
    // The default config often runs at another rate than the output (e.g. a
    // MacBook mic defaults to 48 kHz beside 44.1 kHz speakers), so ask for the
    // output's rate explicitly.
    let cfg = dev
        .supported_input_configs()
        .map_err(|e| e.to_string())?
        .filter(|c| c.sample_format() == cpal::SampleFormat::F32)
        .find_map(|c| c.try_with_sample_rate(rate))
        .ok_or_else(|| format!("input device has no f32 config at {rate} Hz"))?;
    let channels = cfg.channels() as usize;
    let (mut tx, rx) = RingBuffer::<f32>::new(MAX_FRAMES * 2);
    let stream = dev
        .build_input_stream::<f32, _, _>(
            cfg.config(),
            move |data, _| {
                for frame in data.chunks_exact(channels) {
                    if tx.push(frame[0]).is_err() {
                        break;
                    }
                }
            },
            |e| eprintln!("input stream error: {e}"),
            None,
        )
        .map_err(|e| e.to_string())?;
    stream.pause().map_err(|e| e.to_string())?;
    Ok((stream, rx))
}

struct AudioState {
    processor: Processor,
    input: Option<Consumer<f32>>,
    meters: Arc<Meters>,
    mono_in: Vec<f32>,
    stereo_out: Vec<f32>,
    scan_bin: usize,
    /// The view length the current scan pass is for.
    scan_view: usize,
}

impl AudioState {
    fn render(&mut self, data: &mut [f32], channels: usize) {
        for chunk in data.chunks_mut(MAX_FRAMES * channels) {
            let frames = chunk.len() / channels;
            self.fill_input(frames);
            let (inp, out) = (&self.mono_in[..frames], &mut self.stereo_out[..frames * 2]);
            self.processor.process(inp, out);
            for (dst, src) in chunk.chunks_exact_mut(channels).zip(out.as_chunks::<2>().0) {
                match channels {
                    1 => dst[0] = 0.5 * (src[0] + src[1]),
                    _ => {
                        dst[..2].copy_from_slice(src);
                        dst[2..].fill(0.0);
                    }
                }
            }
        }
        self.scan_waveform();
    }

    fn fill_input(&mut self, frames: usize) {
        let buf = &mut self.mono_in[..frames];
        // Drain the ring even when the mic is not the source, so it never backs up.
        let mut got = 0;
        if let Some(rx) = &mut self.input {
            let excess = rx.slots().saturating_sub(MAX_INPUT_BACKLOG + frames);
            if excess > 0
                && let Ok(c) = rx.read_chunk(excess)
            {
                c.commit_all();
            }
            got = rx.pop_partial_slice(buf).0.len();
        }
        match Source::from_u8(self.meters.source.load(Relaxed)) {
            Source::Mic => buf[got..].fill(0.0),
            Source::Off => buf.fill(0.0),
        }
        let peak = buf.iter().fold(0.0f32, |m, x| m.max(x.abs()));
        // Non-negative f32 bit patterns order like their values, so an
        // integer max is a float max.
        self.meters.input_peak.fetch_max(peak.to_bits(), Relaxed);
    }

    fn scan_waveform(&mut self) {
        let buf = &self.processor.engine().buffers()[0];
        let len = self
            .meters
            .view_frames
            .load(Relaxed)
            .clamp(WAVE_BINS, buf.len());
        if len != self.scan_view {
            // Restart so the redraw sweeps once, left to right.
            self.scan_view = len;
            self.scan_bin = 0;
        }
        for _ in 0..(SCAN_BUDGET * WAVE_BINS / len).max(1) {
            let b = self.scan_bin;
            let range = b * len / WAVE_BINS..(b + 1) * len / WAVE_BINS;
            let peak = buf[range].iter().fold(0.0f32, |m, x| m.max(x.abs()));
            self.meters.peaks[b].store(peak.to_bits(), Relaxed);
            self.scan_bin = (b + 1) % WAVE_BINS;
        }
    }
}
