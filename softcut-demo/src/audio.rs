//! Audio side of the demo: cpal streams around a `softcut::rt` processor.
//!
//! The UI controls the engine through the `rt::Handle`. `Meters` carries what
//! `rt` does not: the input source, the input level and a waveform overview.
//! The engine runs a stereo pair of buffers (0 = left, 1 = right) with stereo
//! input.
//!
//! The engine's sample rate is the startup output device's, and stays fixed:
//! buffers, loop points and positions are all counted at it. Devices at other
//! rates are resampled: inputs in their callback, steered against clock drift
//! by a [`DriftServo`]; outputs in the render loop.

use std::sync::atomic::{
    AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering::Relaxed,
};
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use rtrb::{Consumer, Producer, RingBuffer};
use softcut::rt::{self, Handle, Processor};
use softcut::{Engine, EngineConfig, Quirks};

use softcut_fx::{Chain, FxCmd};

use crate::recorder::Recorder;
use crate::resample::{DriftServo, Streamer};

pub const VOICES: usize = 4;
pub const BUFFERS: usize = 2;
pub const WAVE_BINS: usize = 600;
pub const BUFFER_FRAMES: usize = 1 << 21;
const BLOCK: usize = 64;
const MAX_FRAMES: usize = 4096;
/// Buffer samples rescanned for the waveform per callback, over both buffers.
const SCAN_BUDGET: usize = 32768;
/// Input frames kept queued: the input's latency, and the headroom that
/// absorbs callback jitter. Reading waits until this much has arrived, after
/// startup and after any underrun.
const INPUT_FILL: usize = 1024;
/// Input frames queued beyond this are dropped, to bound input latency.
const MAX_INPUT_BACKLOG: usize = 2 * INPUT_FILL;
/// Seconds of output the recording ring holds while the writer catches up.
const RECORD_SECONDS: usize = 2;
/// Resampled output frames buffered between engine blocks.
const PENDING_FRAMES: usize = 4096;
/// Platforms where cpal opens an output device as a system-audio input.
pub const LOOPBACK: bool = cfg!(any(target_os = "macos", target_os = "windows"));

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

/// Which effects chain a command is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FxTarget {
    Voice(usize),
    Master,
}

/// A device the demo can record from.
pub struct InputDevice {
    pub name: String,
    pub is_default: bool,
    /// An output device, captured through the OS's system-audio loopback. It
    /// hears everything playing on that device, softcut included.
    pub loopback: bool,
    device: cpal::Device,
}

/// A device the demo can play through.
pub struct OutputDevice {
    pub name: String,
    pub is_default: bool,
    device: cpal::Device,
}

fn name_of(device: &cpal::Device) -> String {
    device
        .description()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|_| "unnamed device".into())
}

/// Recording devices, default first, then (where supported) output devices
/// as loopback sources.
fn input_devices(host: &cpal::Host) -> Vec<InputDevice> {
    let default = host.default_input_device().and_then(|d| d.id().ok());
    let mut devices: Vec<InputDevice> = host
        .input_devices()
        .map(|it| {
            it.map(|device| InputDevice {
                name: name_of(&device),
                is_default: device.id().ok().is_some_and(|id| Some(id) == default),
                loopback: false,
                device,
            })
            .collect()
        })
        .unwrap_or_default();
    devices.sort_by_key(|d| !d.is_default);
    if LOOPBACK {
        let outputs = host.output_devices().map(|it| it.collect::<Vec<_>>());
        for device in outputs.unwrap_or_default() {
            // A device with inputs of its own records those instead.
            if !device.supports_input() {
                devices.push(InputDevice {
                    name: name_of(&device),
                    is_default: false,
                    loopback: true,
                    device,
                });
            }
        }
    }
    devices
}

/// Playback devices, default first.
fn output_devices(host: &cpal::Host) -> Vec<OutputDevice> {
    let default = host.default_output_device().and_then(|d| d.id().ok());
    let mut devices: Vec<OutputDevice> = host
        .output_devices()
        .map(|it| {
            it.map(|device| OutputDevice {
                name: name_of(&device),
                is_default: device.id().ok().is_some_and(|id| Some(id) == default),
                device,
            })
            .collect()
        })
        .unwrap_or_default();
    devices.sort_by_key(|d| !d.is_default);
    devices
}

/// Shared between the UI and the audio threads. f32 values are stored as bits.
pub struct Meters {
    /// Written by the UI.
    source: AtomicU8,
    /// Absolute peak per bin, per buffer.
    peaks: [[AtomicU32; WAVE_BINS]; BUFFERS],
    /// Highest input level since the UI last took it.
    input_peak: AtomicU32,
    /// Frames received from the input device, before resampling.
    input_frames: AtomicU64,
    /// Input callbacks that held any non-zero sample. macOS delivers exact
    /// silence, not an error, to an app without recording permission.
    input_signal: AtomicU64,
    /// Written by the UI: copy the output mix to the recording ring.
    recording: AtomicBool,
    /// Output samples not recorded because the ring was full.
    rec_dropped: AtomicU64,
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

    /// Frames the input device has delivered so far.
    pub fn input_frames(&self) -> u64 {
        self.input_frames.load(Relaxed)
    }

    /// Output samples lost from recordings because the writer fell behind.
    pub fn rec_dropped(&self) -> u64 {
        self.rec_dropped.load(Relaxed)
    }

    /// Input callbacks so far that were not exactly silent.
    pub fn input_signal(&self) -> u64 {
        self.input_signal.load(Relaxed)
    }
}

/// The open input stream and what it was opened with.
struct Input {
    stream: cpal::Stream,
    device: usize,
    first_channel: usize,
    channels: usize,
    rate: u32,
}

/// The open output stream, its device's name and the rate it runs at.
struct Output {
    stream: cpal::Stream,
    name: String,
    rate: u32,
}

pub struct Audio {
    pub handle: Handle,
    pub meters: Arc<Meters>,
    /// The engine's rate, fixed at startup.
    pub sample_rate: f32,
    pub buffer_seconds: f32,
    pub inputs: Vec<InputDevice>,
    pub outputs: Vec<OutputDevice>,
    /// Why no input is open, if none is.
    pub input_error: Option<String>,
    host: cpal::Host,
    /// Shared so a newly opened input stream takes over the ring the audio
    /// thread reads.
    input_tx: Arc<Mutex<Producer<f32>>>,
    /// Paused unless the source is `On`, so the device is only in use when chosen.
    input: Option<Input>,
    /// Shared so a newly opened output stream takes over rendering. Only
    /// output callbacks lock it, with `try_lock`, so it is contended only
    /// while a switch has two streams open.
    state: Arc<Mutex<AudioState>>,
    output: Output,
    /// The recording ring's reader, held here between recordings.
    rec_rx: Option<Consumer<f32>>,
    recorder: Option<Recorder>,
    fx_tx: Producer<(FxTarget, FxCmd)>,
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

    /// The open input's rate, which differs from `sample_rate` when resampled.
    pub fn input_rate(&self) -> Option<u32> {
        self.input.as_ref().map(|i| i.rate)
    }

    /// Open `inputs[device]`, feeding channels `first_channel` and the one
    /// after (or the same, on a mono device) to L and R. On failure the
    /// previous input stays open.
    pub fn select_input(&mut self, device: usize, first_channel: usize) -> Result<(), String> {
        let dev = self.inputs.get(device).ok_or("no such device")?;
        let on = Source::from_u8(self.meters.source.load(Relaxed)) == Source::On;
        if let Some(old) = &self.input {
            let _ = old.stream.pause();
        }
        let opened = open_input(
            dev,
            self.sample_rate as u32,
            first_channel,
            self.input_tx.clone(),
            self.meters.clone(),
        );
        match opened {
            Ok((stream, channels, rate)) => {
                if on {
                    stream.play().map_err(|e| e.to_string())?;
                }
                self.input = Some(Input {
                    stream,
                    device,
                    first_channel,
                    channels,
                    rate,
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

    /// Queue an effects setting; applied before the next block. A full queue
    /// drops it, as the engine's own command queue does.
    pub fn send_fx(&mut self, target: FxTarget, cmd: FxCmd) {
        let _ = self.fx_tx.push((target, cmd));
    }

    /// Record the output mix to a stereo 32-bit float WAV at `path`.
    pub fn start_recording(&mut self, path: &std::path::Path) -> Result<(), String> {
        let rx = self.rec_rx.take().ok_or("already recording")?;
        match Recorder::start(rx, path, self.sample_rate as u32) {
            Ok(rec) => {
                self.recorder = Some(rec);
                self.meters.rec_dropped.store(0, Relaxed);
                self.meters.recording.store(true, Relaxed);
                Ok(())
            }
            Err((rx, e)) => {
                self.rec_rx = Some(rx);
                Err(e)
            }
        }
    }

    /// Finish the recording; returns its path and length in seconds.
    pub fn stop_recording(&mut self) -> Result<(std::path::PathBuf, f32), String> {
        let rec = self.recorder.take().ok_or("not recording")?;
        self.meters.recording.store(false, Relaxed);
        let path = rec.path.clone();
        let (rx, result) = rec.stop();
        self.rec_rx = Some(rx);
        result.map(|frames| (path, frames as f32 / self.sample_rate))
    }

    /// Seconds recorded so far, while recording.
    pub fn recorded_seconds(&self) -> Option<f32> {
        self.recorder
            .as_ref()
            .map(|r| r.frames() as f32 / self.sample_rate)
    }

    /// Index into `outputs` of the open device, if it is still listed.
    pub fn output_device(&self) -> Option<usize> {
        self.outputs.iter().position(|d| d.name == self.output.name)
    }

    pub fn output_name(&self) -> &str {
        &self.output.name
    }

    /// The open output's rate, which differs from `sample_rate` when resampled.
    pub fn output_rate(&self) -> u32 {
        self.output.rate
    }

    /// Play through `outputs[device]`, resampled if it cannot run at the
    /// engine's rate. On failure the previous output keeps playing.
    pub fn select_output(&mut self, device: usize) -> Result<(), String> {
        let dev = self.outputs.get(device).ok_or("no such device")?;
        let _ = self.output.stream.pause();
        let (stream, rate) =
            match open_output(&dev.device, self.sample_rate as u32, self.state.clone()) {
                Ok(opened) => opened,
                Err(e) => {
                    let _ = self.output.stream.play();
                    return Err(e);
                }
            };
        // Neither stream is running, so this lock is uncontended.
        self.state
            .lock()
            .map_err(|_| "audio state poisoned")?
            .set_output_rate(self.sample_rate as u32, rate);
        stream.play().map_err(|e| e.to_string())?;
        // Dropping the old stream stops its callbacks.
        self.output = Output {
            stream,
            name: dev.name.clone(),
            rate,
        };
        Ok(())
    }

    /// Re-enumerate devices, keeping the open input selected if still present.
    pub fn refresh_devices(&mut self) {
        let open_in = self.input_device().map(|i| self.inputs[i].name.clone());
        self.inputs = input_devices(&self.host);
        self.outputs = output_devices(&self.host);
        if let Some(input) = &mut self.input {
            match self
                .inputs
                .iter()
                .position(|d| Some(&d.name) == open_in.as_ref())
            {
                Some(i) => input.device = i,
                None => self.input_error = Some("the open input is no longer listed".into()),
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

/// Opens the default devices and starts the engine at the default output's
/// rate. `setup` configures the engine before the audio thread takes it.
pub fn start(setup: impl FnOnce(&mut Engine)) -> Result<Audio, String> {
    let host = cpal::default_host();
    let outputs = output_devices(&host);
    let out = outputs.first().ok_or("no output device")?;
    let out_dev = &out.device;
    let output_name = out.name.clone();
    let sample_rate = out_dev
        .default_output_config()
        .map_err(|e| e.to_string())?
        .sample_rate();

    let mut engine = engine(sample_rate as f32);
    setup(&mut engine);
    let (handle, processor) = rt::split(engine, 1024);

    let (input_tx, input_rx) = RingBuffer::<f32>::new(MAX_FRAMES * 4);
    let (rec_tx, rec_rx) = RingBuffer::<f32>::new(sample_rate as usize * 2 * RECORD_SECONDS);
    let (fx_tx, fx_rx) = RingBuffer::new(1024);
    let meters = Arc::new(Meters {
        source: AtomicU8::new(Source::Off as u8),
        peaks: std::array::from_fn(|_| std::array::from_fn(|_| AtomicU32::new(0))),
        input_peak: AtomicU32::new(0),
        input_frames: AtomicU64::new(0),
        input_signal: AtomicU64::new(0),
        recording: AtomicBool::new(false),
        rec_dropped: AtomicU64::new(0),
        view_frames: AtomicUsize::new(BUFFER_FRAMES),
    });
    let state = Arc::new(Mutex::new(AudioState::new(
        processor,
        input_rx,
        rec_tx,
        fx_rx,
        meters.clone(),
        sample_rate as f32,
    )));
    let (stream, out_rate) = open_output(out_dev, sample_rate, state.clone())?;
    state
        .lock()
        .map_err(|_| "audio state poisoned")?
        .set_output_rate(sample_rate, out_rate);
    stream.play().map_err(|e| e.to_string())?;

    let mut audio = Audio {
        handle,
        meters,
        sample_rate: sample_rate as f32,
        buffer_seconds: BUFFER_FRAMES as f32 / sample_rate as f32,
        inputs: input_devices(&host),
        outputs,
        input_error: None,
        host,
        input_tx: Arc::new(Mutex::new(input_tx)),
        input: None,
        state,
        output: Output {
            stream,
            name: output_name,
            rate: out_rate,
        },
        rec_rx: Some(rec_rx),
        recorder: None,
        fx_tx,
    };
    audio.input_error = match audio.inputs.first() {
        Some(d) if !d.loopback => audio.select_input(0, 0).err(),
        _ => Some("no input device".into()),
    };
    Ok(audio)
}

/// Opens `dev`, not yet playing, rendering from `state`: at `rate` if the
/// device offers it, else at its nearest rate. Prefers a stereo layout.
/// Returns the stream and the rate it runs at.
fn open_output(
    dev: &cpal::Device,
    rate: cpal::SampleRate,
    state: Arc<Mutex<AudioState>>,
) -> Result<(cpal::Stream, cpal::SampleRate), String> {
    let cfg = dev
        .supported_output_configs()
        .map_err(|e| e.to_string())?
        .filter(|c| c.sample_format() == cpal::SampleFormat::F32)
        .map(|c| {
            let r = rate.clamp(c.min_sample_rate(), c.max_sample_rate());
            (
                r == rate,
                c.channels() == 2,
                c.channels(),
                c.with_sample_rate(r),
            )
        })
        .max_by_key(|&(exact, stereo, channels, _)| (exact, stereo, channels))
        .map(|(.., c)| c)
        .ok_or("device has no f32 output")?;
    let channels = cfg.channels() as usize;
    let stream = dev
        .build_output_stream::<f32, _, _>(
            cfg.config(),
            move |data, _| match state.try_lock() {
                Ok(mut s) => s.render(data, channels),
                // Only while a switch has two streams open.
                Err(_) => data.fill(0.0),
            },
            |e| eprintln!("output stream error: {e}"),
            None,
        )
        .map_err(|e| e.to_string())?;
    stream.pause().map_err(|e| e.to_string())?;
    Ok((stream, cfg.sample_rate()))
}

/// Opens `dev`, paused, forwarding channels `first` and `first + 1` (or
/// `first` twice on a mono device) as interleaved frames into `tx`, resampled
/// to `rate` if the device cannot run at it. Returns the stream, the device's
/// channel count and the rate it runs at.
fn open_input(
    dev: &InputDevice,
    rate: cpal::SampleRate,
    first: usize,
    tx: Arc<Mutex<Producer<f32>>>,
    meters: Arc<Meters>,
) -> Result<(cpal::Stream, usize, cpal::SampleRate), String> {
    let configs: Vec<cpal::SupportedStreamConfigRange> = if dev.loopback {
        dev.device.supported_output_configs().map(|c| c.collect())
    } else {
        dev.device.supported_input_configs().map(|c| c.collect())
    }
    .map_err(|e| e.to_string())?;
    let configs = configs
        .into_iter()
        .filter(|c| c.sample_format() == cpal::SampleFormat::F32);
    // The engine's rate if offered, so nothing is resampled; otherwise the
    // nearest rate the device supports. Either way, the most channels.
    let cfg = configs
        .map(|c| {
            let r = rate.clamp(c.min_sample_rate(), c.max_sample_rate());
            (r == rate, c.channels(), c.with_sample_rate(r))
        })
        .max_by_key(|&(exact, channels, _)| (exact, channels))
        .map(|(_, _, c)| c)
        .ok_or("device has no f32 input")?;
    let channels = cfg.channels() as usize;
    if first >= channels {
        let plural = if channels == 1 { "" } else { "s" };
        return Err(format!("device has {channels} input channel{plural}"));
    }
    let dev_rate = cfg.sample_rate();
    let mut resampler = (dev_rate != rate).then(|| Streamer::new(dev_rate as f64, rate as f64));
    let mut servo = DriftServo::new(INPUT_FILL as f64);
    let (l, r) = (first, (first + 1).min(channels - 1));
    let stream = dev
        .device
        .build_input_stream::<f32, _, _>(
            cfg.config(),
            move |data, _| {
                meters
                    .input_frames
                    .fetch_add((data.len() / channels) as u64, Relaxed);
                if data.iter().any(|&x| x != 0.0) {
                    meters.input_signal.fetch_add(1, Relaxed);
                }
                // Never blocks: contended only while a switch has two streams open.
                let Ok(mut tx) = tx.try_lock() else { return };
                if let Some(s) = &mut resampler {
                    let fill = (tx.buffer().capacity() - tx.slots()) / 2;
                    let dt = (data.len() / channels) as f64 / dev_rate as f64;
                    s.set_adjust(servo.update(fill as f64, dt));
                }
                for frame in data.chunks_exact(channels) {
                    // Both samples or neither, so the ring never splits a frame;
                    // a full ring drops the frame.
                    let lr = [frame[l], frame[r]];
                    match &mut resampler {
                        Some(s) => s.push(lr, |out| {
                            let _ = tx.push_entire_slice(&out);
                        }),
                        None => {
                            let _ = tx.push_entire_slice(&lr);
                        }
                    }
                }
            },
            |e| eprintln!("input stream error: {e}"),
            None,
        )
        .map_err(|e| e.to_string())?;
    stream.pause().map_err(|e| e.to_string())?;
    Ok((stream, channels, dev_rate))
}

/// Copy a rendered block to the recording ring while recording: all of it or
/// none, so the file never loses one channel of a frame. A full ring drops
/// the block rather than waiting for the writer.
fn record(meters: &Meters, tx: &mut Producer<f32>, block: &[f32]) {
    if meters.recording.load(Relaxed) && tx.push_entire_slice(block).is_err() {
        meters.rec_dropped.fetch_add(block.len() as u64, Relaxed);
    }
}

/// Write a stereo frame to a device frame: mixed on mono, into the first two
/// channels otherwise.
fn write_frame(dst: &mut [f32], lr: [f32; 2]) {
    match dst.len() {
        1 => dst[0] = 0.5 * (lr[0] + lr[1]),
        _ => {
            dst[..2].copy_from_slice(&lr);
            dst[2..].fill(0.0);
        }
    }
}

struct AudioState {
    processor: Processor,
    input: Consumer<f32>,
    /// Output mix frames for the recorder, while `Meters::recording` is set.
    rec_tx: Producer<f32>,
    fx_rx: Consumer<(FxTarget, FxCmd)>,
    /// A mono insert chain per voice, and a stereo chain on the mix.
    voice_fx: Vec<Chain>,
    master_fx: Chain,
    /// False until `INPUT_FILL` frames have queued, at startup and after an underrun.
    primed: bool,
    meters: Arc<Meters>,
    stereo_in: Vec<f32>,
    stereo_out: Vec<f32>,
    /// Engine rate to device rate, when they differ.
    out_resampler: Option<Streamer>,
    /// Resampled frames not yet written to the device, from `pending_pos` on.
    pending: Vec<[f32; 2]>,
    pending_pos: usize,
    scan_bin: usize,
    /// The view length the current scan pass is for.
    scan_view: usize,
}

impl AudioState {
    fn new(
        processor: Processor,
        input: Consumer<f32>,
        rec_tx: Producer<f32>,
        fx_rx: Consumer<(FxTarget, FxCmd)>,
        meters: Arc<Meters>,
        sample_rate: f32,
    ) -> Self {
        Self {
            processor,
            input,
            rec_tx,
            fx_rx,
            voice_fx: (0..VOICES).map(|_| Chain::new(sample_rate, 1)).collect(),
            master_fx: Chain::new(sample_rate, 2),
            primed: false,
            meters,
            stereo_in: vec![0.0; MAX_FRAMES * 2],
            stereo_out: vec![0.0; MAX_FRAMES * 2],
            out_resampler: None,
            pending: Vec::with_capacity(PENDING_FRAMES),
            pending_pos: 0,
            scan_bin: 0,
            scan_view: 0,
        }
    }

    /// Resample to `device` Hz from the engine's `engine` Hz, or stop resampling.
    fn set_output_rate(&mut self, engine: u32, device: u32) {
        self.out_resampler =
            (engine != device).then(|| Streamer::new(engine as f64, device as f64));
        self.pending.clear();
        self.pending_pos = 0;
    }

    fn render(&mut self, data: &mut [f32], channels: usize) {
        while let Ok((target, cmd)) = self.fx_rx.pop() {
            match target {
                FxTarget::Voice(v) if v < VOICES => self.voice_fx[v].apply(cmd),
                FxTarget::Voice(_) => {}
                FxTarget::Master => self.master_fx.apply(cmd),
            }
        }
        if self.out_resampler.is_some() {
            self.render_resampled(data, channels);
        } else {
            for chunk in data.chunks_mut(MAX_FRAMES * channels) {
                let frames = chunk.len() / channels;
                self.fill_input(frames);
                let (inp, out) = (
                    &self.stereo_in[..frames * 2],
                    &mut self.stereo_out[..frames * 2],
                );
                let voice_fx = &mut self.voice_fx;
                self.processor
                    .process_with(inp, out, |v, block| voice_fx[v].process(block));
                self.master_fx.process(out);
                record(&self.meters, &mut self.rec_tx, out);
                for (dst, src) in chunk.chunks_exact_mut(channels).zip(out.as_chunks::<2>().0) {
                    write_frame(dst, *src);
                }
            }
        }
        self.scan_waveform();
    }

    /// Render engine blocks at the engine's rate as the device asks for
    /// frames, resampling each into `pending`.
    fn render_resampled(&mut self, data: &mut [f32], channels: usize) {
        for dst in data.chunks_exact_mut(channels) {
            // A block can emit nothing while the kernel's history fills.
            while self.pending_pos == self.pending.len() {
                self.pending.clear();
                self.pending_pos = 0;
                self.fill_input(BLOCK);
                let (inp, out) = (
                    &self.stereo_in[..BLOCK * 2],
                    &mut self.stereo_out[..BLOCK * 2],
                );
                let voice_fx = &mut self.voice_fx;
                self.processor
                    .process_with(inp, out, |v, block| voice_fx[v].process(block));
                self.master_fx.process(out);
                record(&self.meters, &mut self.rec_tx, out);
                let (Some(rs), pending) = (self.out_resampler.as_mut(), &mut self.pending) else {
                    unreachable!("render_resampled without a resampler")
                };
                for frame in self.stereo_out[..BLOCK * 2].as_chunks::<2>().0 {
                    rs.push(*frame, |o| {
                        // Never reallocate on the audio thread.
                        if pending.len() < pending.capacity() {
                            pending.push(o);
                        }
                    });
                }
            }
            write_frame(dst, self.pending[self.pending_pos]);
            self.pending_pos += 1;
        }
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
        if !self.primed && rx.slots() >= 2 * INPUT_FILL {
            self.primed = true;
        }
        let got = if self.primed {
            rx.pop_partial_slice(buf).0.len()
        } else {
            0
        };
        if got < buf.len() {
            // Underrun: wait for the queue to refill before reading again.
            self.primed = false;
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use softcut::{EngineCmd, VoiceCmd};

    /// Effect commands reach the chains: a 1-bit crusher rounds a quiet
    /// voice to silence, on its own insert or on the master.
    #[test]
    fn effects_apply_to_voice_inserts_and_master() {
        use softcut_fx::Fx;
        let render_peak = |target: Option<FxTarget>| {
            let mut e = engine(44100.0);
            e.buffer_mut(0).fill(0.2);
            for c in [
                VoiceCmd::LoopEnd(1.0),
                VoiceCmd::Loop(true),
                VoiceCmd::Play(true),
            ] {
                e.apply(EngineCmd::Voice(0, c));
            }
            let (mut st, _tx, _rec, mut fx) = state_all(e);
            if let Some(t) = target {
                fx.push((t, FxCmd::Enable(Fx::Bitcrusher, true))).unwrap();
                fx.push((t, FxCmd::Bits(1.0))).unwrap();
            }
            let mut data = vec![0.0; 512 * 2];
            (0..4).fold(0.0f32, |m, _| {
                st.render(&mut data, 2);
                data.iter().fold(m, |m, x| m.max(x.abs()))
            })
        };
        assert!(render_peak(None) > 0.1);
        assert_eq!(render_peak(Some(FxTarget::Master)), 0.0);
        assert_eq!(render_peak(Some(FxTarget::Voice(0))), 0.0);
        // A chain on another voice leaves voice 0 alone.
        assert!(render_peak(Some(FxTarget::Voice(1))) > 0.1);
    }

    fn state(engine: Engine) -> (AudioState, Producer<f32>) {
        let (st, tx, ..) = state_all(engine);
        (st, tx)
    }

    fn state_rec(engine: Engine) -> (AudioState, Producer<f32>, Consumer<f32>) {
        let (st, tx, rec, _) = state_all(engine);
        (st, tx, rec)
    }

    type FxSender = Producer<(FxTarget, FxCmd)>;

    /// The state, the input ring's writer, the recording ring's reader and
    /// the effects queue's writer.
    fn state_all(engine: Engine) -> (AudioState, Producer<f32>, Consumer<f32>, FxSender) {
        let (tx, rx) = RingBuffer::new(MAX_FRAMES * 4);
        let (rec_tx, rec_rx) = RingBuffer::new(1 << 16);
        let (fx_tx, fx_rx) = RingBuffer::new(16);
        let meters = Arc::new(Meters {
            source: AtomicU8::new(Source::On as u8),
            peaks: std::array::from_fn(|_| std::array::from_fn(|_| AtomicU32::new(0))),
            input_peak: AtomicU32::new(0),
            input_frames: AtomicU64::new(0),
            input_signal: AtomicU64::new(0),
            recording: AtomicBool::new(false),
            rec_dropped: AtomicU64::new(0),
            view_frames: AtomicUsize::new(BUFFER_FRAMES),
        });
        let (_handle, processor) = rt::split(engine, 16);
        (
            AudioState::new(processor, rx, rec_tx, fx_rx, meters, 44100.0),
            tx,
            rec_rx,
            fx_tx,
        )
    }

    /// While recording, both render paths copy exactly the engine frames they
    /// render: all of the direct path's, and the resampled path's at the
    /// engine rate. Nothing is copied while not recording.
    #[test]
    fn recording_copies_the_rendered_mix() {
        for device_rate in [44100, 48000] {
            let mut e = engine(44100.0);
            e.buffer_mut(0).fill(0.25);
            for c in [
                VoiceCmd::LoopEnd(1.0),
                VoiceCmd::Loop(true),
                VoiceCmd::Play(true),
            ] {
                e.apply(EngineCmd::Voice(0, c));
            }
            let (mut st, _tx, mut rec) = state_rec(e);
            st.set_output_rate(44100, device_rate);
            let mut data = vec![0.0; 512 * 2];
            st.render(&mut data, 2);
            assert_eq!(rec.slots(), 0, "copied while not recording");

            st.meters.recording.store(true, Relaxed);
            for _ in 0..10 {
                st.render(&mut data, 2);
            }
            let got = rec.slots();
            let chunk = rec.read_chunk(got).unwrap();
            let (a, b) = chunk.as_slices();
            let samples: Vec<f32> = a.iter().chain(b).copied().collect();
            chunk.commit_all();
            assert_eq!(got % 2, 0);
            let frames = got / 2;
            // Direct: exactly the 5120 device frames. Resampled: engine
            // blocks of 64 covering 5120 device frames at 44.1/48.
            let want = if device_rate == 44100 {
                5120
            } else {
                5120 * 44100 / 48000
            };
            assert!(
                frames.abs_diff(want) <= 128,
                "{device_rate} Hz: {frames} frames"
            );
            assert!(
                samples[1000..].iter().any(|&x| x != 0.0),
                "silent recording"
            );
            assert_eq!(st.meters.rec_dropped(), 0);
        }
    }

    /// A 1 kHz loop rendered from a 44.1 kHz engine to a 48 kHz device stays
    /// 1 kHz, with no jump larger than the sine's own step.
    #[test]
    fn output_resampling_keeps_pitch_and_continuity() {
        let mut e = engine(44100.0);
        for (i, x) in e.buffer_mut(0).iter_mut().enumerate() {
            *x = 0.5 * (std::f32::consts::TAU * 1000.0 * i as f32 / 44100.0).sin();
        }
        // One second is exactly 1000 cycles, so the loop has no seam.
        for c in [
            VoiceCmd::LoopEnd(1.0),
            VoiceCmd::Loop(true),
            VoiceCmd::Play(true),
        ] {
            e.apply(EngineCmd::Voice(0, c));
        }
        e.apply(EngineCmd::Pan(0, -1.0));
        let (mut st, _tx) = state(e);
        st.set_output_rate(44100, 48000);

        let mut left = Vec::new();
        let mut data = vec![0.0; 512 * 2];
        for _ in 0..(48000 / 512) {
            st.render(&mut data, 2);
            left.extend(data.as_chunks::<2>().0.iter().map(|f| f[0]));
        }
        let settled = &left[1000..];
        let crossings = settled
            .windows(2)
            .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
            .count();
        let seconds = settled.len() as f32 / 48000.0;
        let hz = crossings as f32 / 2.0 / seconds;
        assert!((hz - 1000.0).abs() < 5.0, "{hz} Hz");
        let step = 0.5 * std::f32::consts::TAU * 1000.0 / 48000.0;
        let jump = settled
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0, f32::max);
        assert!(jump < 1.05 * step, "jump {jump} vs step {step}");
    }

    #[test]
    fn input_waits_until_primed_and_after_underrun() {
        let (mut st, mut tx) = state(engine(44100.0));
        let push = |tx: &mut Producer<f32>, frames: usize| {
            for _ in 0..frames {
                tx.push_entire_slice(&[0.5, 0.5]).unwrap();
            }
        };
        push(&mut tx, INPUT_FILL - 1);
        st.fill_input(256);
        assert!(
            st.stereo_in[..512].iter().all(|&x| x == 0.0),
            "read before primed"
        );
        push(&mut tx, 1);
        st.fill_input(256);
        assert!(st.stereo_in[..512].iter().all(|&x| x == 0.5), "primed read");
        // 768 frames left; ask for more to underrun.
        st.fill_input(1000);
        assert!(!st.primed);
        push(&mut tx, 100);
        st.fill_input(64);
        assert!(
            st.stereo_in[..128].iter().all(|&x| x == 0.0),
            "read while refilling"
        );
    }
}
