//! Audio side of the demo: cpal streams around a `softcut::rt` processor.
//!
//! The UI controls the engine through the `rt::Handle`. `Meters` carries what
//! `rt` does not: the input source, the input level and a waveform overview.
//! The engine runs a stereo pair of buffers (0 = left, 1 = right) with stereo
//! input.

use std::sync::atomic::{AtomicU8, AtomicU32, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use rtrb::{Consumer, Producer, RingBuffer};
use softcut::rt::{self, Handle, Processor};
use softcut::{Engine, EngineConfig, Quirks};

pub const VOICES: usize = 4;
pub const BUFFERS: usize = 2;
pub const WAVE_BINS: usize = 600;
pub const BUFFER_FRAMES: usize = 1 << 21;
const BLOCK: usize = 64;
const MAX_FRAMES: usize = 4096;
/// Buffer samples rescanned for the waveform per callback, over both buffers.
const SCAN_BUDGET: usize = 32768;
/// Input frames queued beyond this are dropped, to bound mic latency.
const MAX_INPUT_BACKLOG: usize = 2048;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Source {
    /// The selected input device.
    On,
    /// Input stream paused; the engine gets zeros.
    Off,
}

impl Source {
    fn from_u8(x: u8) -> Self {
        if x == Source::On as u8 {
            Source::On
        } else {
            Source::Off
        }
    }
}

/// A recording device the demo can open.
pub struct InputDevice {
    pub name: String,
    pub is_default: bool,
    device: cpal::Device,
}

/// Input devices, default first.
fn input_devices(host: &cpal::Host) -> Vec<InputDevice> {
    let default = host.default_input_device().and_then(|d| d.id().ok());
    let mut devices: Vec<InputDevice> = host
        .input_devices()
        .map(|it| {
            it.map(|device| InputDevice {
                name: device
                    .description()
                    .map(|d| d.name().to_string())
                    .unwrap_or_else(|_| "unnamed device".into()),
                is_default: device.id().ok().is_some_and(|id| Some(id) == default),
                device,
            })
            .collect()
        })
        .unwrap_or_default();
    devices.sort_by_key(|d| !d.is_default);
    devices
}

/// Shared between the UI and the audio thread. f32 values are stored as bits.
pub struct Meters {
    /// Written by the UI.
    source: AtomicU8,
    /// Absolute peak per bin, per buffer.
    peaks: [[AtomicU32; WAVE_BINS]; BUFFERS],
    /// Highest input level since the UI last took it.
    input_peak: AtomicU32,
    /// Frames from the buffer start that the waveform bins span. Written by the UI.
    view_frames: AtomicUsize,
}

impl Meters {
    pub fn set_view_frames(&self, frames: usize) {
        self.view_frames.store(frames, Relaxed);
    }

    pub fn peak(&self, buffer: usize, bin: usize) -> f32 {
        f32::from_bits(self.peaks[buffer][bin].load(Relaxed))
    }

    /// Highest input level since the last call; resets it.
    pub fn take_input_peak(&self) -> f32 {
        f32::from_bits(self.input_peak.swap(0, Relaxed))
    }
}

/// The open input stream and what it was opened with.
struct Input {
    stream: cpal::Stream,
    device: usize,
    first_channel: usize,
    channels: usize,
}

pub struct Audio {
    pub handle: Handle,
    pub meters: Arc<Meters>,
    pub sample_rate: f32,
    pub buffer_seconds: f32,
    pub output_name: String,
    pub inputs: Vec<InputDevice>,
    /// Why no input is open, if none is.
    pub input_error: Option<String>,
    host: cpal::Host,
    /// Shared so a newly opened stream takes over the ring the audio thread reads.
    input_tx: Arc<Mutex<Producer<f32>>>,
    /// Paused unless the source is `On`, so the device is only in use when chosen.
    input: Option<Input>,
    _output_stream: cpal::Stream,
}

impl Audio {
    /// Switch the input on or off, starting or pausing its stream to match.
    pub fn set_source(&mut self, s: Source) -> Result<(), String> {
        if let Some(input) = &self.input {
            match s {
                Source::On => input.stream.play(),
                Source::Off => input.stream.pause(),
            }
            .map_err(|e| e.to_string())?;
        } else if s == Source::On {
            return Err(self.input_error.clone().unwrap_or_default());
        }
        self.meters.source.store(s as u8, Relaxed);
        Ok(())
    }

    /// Index into `inputs` of the open device, if one is open.
    pub fn input_device(&self) -> Option<usize> {
        self.input.as_ref().map(|i| i.device)
    }

    /// First channel of the open stereo pair, and the device's channel count.
    pub fn input_channels(&self) -> (usize, usize) {
        self.input
            .as_ref()
            .map_or((0, 0), |i| (i.first_channel, i.channels))
    }

    /// Open `inputs[device]`, feeding channels `first_channel` and the one
    /// after (or the same, on a mono device) to L and R. On failure the
    /// previous input stays open.
    pub fn select_input(&mut self, device: usize, first_channel: usize) -> Result<(), String> {
        let dev = &self.inputs.get(device).ok_or("no such device")?.device;
        let on = Source::from_u8(self.meters.source.load(Relaxed)) == Source::On;
        if let Some(old) = &self.input {
            let _ = old.stream.pause();
        }
        let opened = open_input(
            dev,
            self.sample_rate as u32,
            first_channel,
            self.input_tx.clone(),
        );
        match opened {
            Ok((stream, channels)) => {
                if on {
                    stream.play().map_err(|e| e.to_string())?;
                }
                self.input = Some(Input {
                    stream,
                    device,
                    first_channel,
                    channels,
                });
                self.input_error = None;
                Ok(())
            }
            Err(e) => {
                if on && let Some(old) = &self.input {
                    let _ = old.stream.play();
                }
                Err(e)
            }
        }
    }

    /// Re-enumerate devices, keeping the open one selected if still present.
    pub fn refresh_inputs(&mut self) {
        let open = self.input_device().map(|i| self.inputs[i].name.clone());
        self.inputs = input_devices(&self.host);
        if let Some(input) = &mut self.input {
            match self
                .inputs
                .iter()
                .position(|d| Some(&d.name) == open.as_ref())
            {
                Some(i) => input.device = i,
                None => self.input_error = Some("the open device is no longer listed".into()),
            }
        }
    }
}

/// The engine the demo runs, before the audio thread takes it.
pub fn engine(sample_rate: f32) -> Engine {
    Engine::new(EngineConfig {
        sample_rate,
        voices: VOICES,
        buffers: BUFFERS,
        buffer_frames: BUFFER_FRAMES,
        block_size: BLOCK,
        in_channels: 2,
        out_channels: 2,
        // Saved loops should keep the input's polarity.
        quirks: Quirks::Fixed,
    })
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

    let mut engine = engine(sample_rate);
    setup(&mut engine);

    let (handle, processor) = rt::split(engine, 1024);

    let (input_tx, input_rx) = RingBuffer::<f32>::new(MAX_FRAMES * 4);
    let meters = Arc::new(Meters {
        source: AtomicU8::new(Source::Off as u8),
        peaks: std::array::from_fn(|_| std::array::from_fn(|_| AtomicU32::new(0))),
        input_peak: AtomicU32::new(0),
        view_frames: AtomicUsize::new(BUFFER_FRAMES),
    });

    let mut state = AudioState {
        processor,
        input: input_rx,
        meters: meters.clone(),
        stereo_in: vec![0.0; MAX_FRAMES * 2],
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

    let mut audio = Audio {
        handle,
        meters,
        sample_rate,
        buffer_seconds: BUFFER_FRAMES as f32 / sample_rate,
        output_name,
        inputs: input_devices(&host),
        input_error: None,
        host,
        input_tx: Arc::new(Mutex::new(input_tx)),
        input: None,
        _output_stream: stream,
    };
    audio.input_error = match audio.inputs.is_empty() {
        true => Some("no input device".into()),
        false => audio.select_input(0, 0).err(),
    };
    Ok(audio)
}

/// Opens `dev`, paused, at the output's sample rate, forwarding channels
/// `first` and `first + 1` (or `first` twice on a mono device) as interleaved
/// frames into `tx`. Returns the stream and the device's channel count.
/// softcut does no rate conversion on its input.
fn open_input(
    dev: &cpal::Device,
    rate: cpal::SampleRate,
    first: usize,
    tx: Arc<Mutex<Producer<f32>>>,
) -> Result<(cpal::Stream, usize), String> {
    // A device's default config often runs at another rate than the output
    // (e.g. a MacBook mic defaults to 48 kHz beside 44.1 kHz speakers), so
    // ask for the output's rate explicitly, with as many channels as offered.
    let cfg = dev
        .supported_input_configs()
        .map_err(|e| e.to_string())?
        .filter(|c| c.sample_format() == cpal::SampleFormat::F32)
        .filter_map(|c| c.try_with_sample_rate(rate))
        .max_by_key(|c| c.channels())
        .ok_or_else(|| format!("device has no f32 input at {rate} Hz"))?;
    let channels = cfg.channels() as usize;
    if first >= channels {
        let plural = if channels == 1 { "" } else { "s" };
        return Err(format!("device has {channels} input channel{plural}"));
    }
    let (l, r) = (first, (first + 1).min(channels - 1));
    let stream = dev
        .build_input_stream::<f32, _, _>(
            cfg.config(),
            move |data, _| {
                // Never blocks: contended only while a switch has two streams open.
                let Ok(mut tx) = tx.try_lock() else { return };
                for frame in data.chunks_exact(channels) {
                    // Both samples or neither, so the ring never splits a frame.
                    if tx.push_entire_slice(&[frame[l], frame[r]]).is_err() {
                        break;
                    }
                }
            },
            |e| eprintln!("input stream error: {e}"),
            None,
        )
        .map_err(|e| e.to_string())?;
    stream.pause().map_err(|e| e.to_string())?;
    Ok((stream, channels))
}

struct AudioState {
    processor: Processor,
    input: Consumer<f32>,
    meters: Arc<Meters>,
    stereo_in: Vec<f32>,
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
            let (inp, out) = (
                &self.stereo_in[..frames * 2],
                &mut self.stereo_out[..frames * 2],
            );
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
        let buf = &mut self.stereo_in[..frames * 2];
        // Drain the ring even when the input is off, so it never backs up.
        let rx = &mut self.input;
        // Whole frames only: the producer commits samples in pairs.
        let excess = rx.slots().saturating_sub(2 * (MAX_INPUT_BACKLOG + frames)) & !1;
        if excess > 0
            && let Ok(c) = rx.read_chunk(excess)
        {
            c.commit_all();
        }
        let got = rx.pop_partial_slice(buf).0.len();
        match Source::from_u8(self.meters.source.load(Relaxed)) {
            Source::On => buf[got..].fill(0.0),
            Source::Off => buf.fill(0.0),
        }
        let peak = buf.iter().fold(0.0f32, |m, x| m.max(x.abs()));
        // Non-negative f32 bit patterns order like their values, so an
        // integer max is a float max.
        self.meters.input_peak.fetch_max(peak.to_bits(), Relaxed);
    }

    fn scan_waveform(&mut self) {
        let buffers = self.processor.engine().buffers();
        let len = self
            .meters
            .view_frames
            .load(Relaxed)
            .clamp(WAVE_BINS, buffers[0].len());
        if len != self.scan_view {
            // Restart so the redraw sweeps once, left to right.
            self.scan_view = len;
            self.scan_bin = 0;
        }
        for _ in 0..(SCAN_BUDGET * WAVE_BINS / (len * BUFFERS)).max(1) {
            let b = self.scan_bin;
            let range = b * len / WAVE_BINS..(b + 1) * len / WAVE_BINS;
            for (peaks, buf) in self.meters.peaks.iter().zip(buffers) {
                let peak = buf[range.clone()]
                    .iter()
                    .fold(0.0f32, |m, x| m.max(x.abs()));
                peaks[b].store(peak.to_bits(), Relaxed);
            }
            self.scan_bin = (b + 1) % WAVE_BINS;
        }
    }
}
