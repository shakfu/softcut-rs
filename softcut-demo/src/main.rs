//! softcut demo: four voices over a stereo pair of buffers (L and R), with
//! both buffers' waveforms, each voice's loop region and playhead drawn live.
//! Voices 1+2 and 3+4 are linked stereo pairs by default.
//!
//! Waveform: click to cut the selected voice, drag to set its loop.
//! WAV files load into the buffers from the button or by dropping them on the
//! window; "save loop..." saves the selected voice's loop region.

// Per-voice state lives in parallel arrays indexed by voice number.
#![allow(clippy::needless_range_loop)]

mod audio;
mod recorder;
mod resample;
mod wav;

use std::path::{Path, PathBuf};

use audio::{Audio, BUFFER_FRAMES, BUFFERS, FxTarget, InputDevice, Source, VOICES, WAVE_BINS};
use eframe::egui::{self, Color32, Pos2, Rect, Sense, Stroke, Vec2};
use softcut::rt::Returned;
use softcut::{Engine, EngineCmd, FadeShape, Quirks, Voice, VoiceCmd};
use softcut_fx::{Fx, FxCmd};
use softcut_osc::{Action, PhasePoll};

const COLORS: [Color32; VOICES] = [
    Color32::from_rgb(230, 90, 80),
    Color32::from_rgb(80, 170, 230),
    Color32::from_rgb(120, 200, 110),
    Color32::from_rgb(220, 180, 60),
];
/// Edge fade for "clear loop" and "reverse loop", in seconds.
const EDIT_FADE: f32 = 0.005;

/// The UI's copy of each voice's settings. The voice lives on the audio
/// thread, so the UI is the source of truth and pushes every change as a
/// command.
#[derive(Clone, Copy, PartialEq, Debug)]
struct VoiceUi {
    play: bool,
    rec: bool,
    loop_on: bool,
    rate: f32,
    loop_start: f32,
    loop_end: f32,
    fade_time: f32,
    rate_slew: f32,
    rec_level: f32,
    pre_level: f32,
    level: f32,
    pan: f32,
    input_gain: f32,
    post_fc: f32,
    post_rq: f32,
    post_lp: f32,
    post_hp: f32,
    post_bp: f32,
    post_dry: f32,
    rec_fade_shape: FadeShape,
    pre_fade_shape: FadeShape,
    rec_delay_ratio: f32,
    pre_window_ratio: f32,
    /// 0 = left buffer, 1 = right. The voice records the matching input channel.
    buffer: usize,
}

impl VoiceUi {
    fn preset(i: usize) -> Self {
        if i % 2 == 1 {
            return Self::preset(i - 1).partner();
        }
        let base = Self {
            play: true,
            rec: false,
            loop_on: true,
            rate: 1.0,
            loop_start: 0.0,
            loop_end: 4.0,
            fade_time: 0.05,
            rate_slew: 0.1,
            rec_level: 1.0,
            pre_level: 0.5,
            level: 0.8,
            pan: -1.0,
            input_gain: 0.0,
            post_fc: 8000.0,
            post_rq: 2.0,
            post_lp: 0.0,
            post_hp: 0.0,
            post_bp: 0.0,
            post_dry: 1.0,
            rec_fade_shape: FadeShape::Raised,
            pre_fade_shape: FadeShape::Linear,
            rec_delay_ratio: 1.0 / 128.0,
            pre_window_ratio: 1.0 / 8.0,
            buffer: 0,
        };
        match i {
            0 => Self {
                input_gain: 1.0,
                ..base
            },
            _ => Self {
                rate: -0.5,
                level: 0.6,
                post_lp: 1.0,
                post_dry: 0.0,
                post_fc: 2000.0,
                ..base
            },
        }
    }

    /// The other half of a stereo pair: same settings, other buffer, mirrored pan.
    fn partner(&self) -> Self {
        Self {
            pan: -self.pan,
            buffer: BUFFERS - 1 - self.buffer,
            ..*self
        }
    }

    /// Follow a command sent from elsewhere (OSC), for the settings the UI shows.
    fn mirror(&mut self, c: &VoiceCmd) {
        use VoiceCmd::*;
        match *c {
            Rate(x) => self.rate = x,
            LoopStart(x) => self.loop_start = x,
            LoopEnd(x) => self.loop_end = x,
            Loop(b) => self.loop_on = b,
            FadeTime(x) => self.fade_time = x,
            RateSlewTime(x) => self.rate_slew = x,
            RecLevel(x) => self.rec_level = x,
            PreLevel(x) => self.pre_level = x,
            Play(b) => self.play = b,
            Rec(b) => self.rec = b,
            PostFilterFc(x) => self.post_fc = x,
            PostFilterRq(x) => self.post_rq = x,
            PostFilterLp(x) => self.post_lp = x,
            PostFilterHp(x) => self.post_hp = x,
            PostFilterBp(x) => self.post_bp = x,
            PostFilterDry(x) => self.post_dry = x,
            RecFadeShape(x) => self.rec_fade_shape = x,
            PreFadeShape(x) => self.pre_fade_shape = x,
            RecDelayRatio(x) => self.rec_delay_ratio = x,
            PreWindowRatio(x) => self.pre_window_ratio = x,
            _ => {}
        }
    }

    /// Commands that take a voice from `old` to `self`; everything if `old` is None.
    fn diff(&self, i: usize, old: Option<&Self>, out: &mut Vec<EngineCmd>) {
        use VoiceCmd::*;
        let v = |c| EngineCmd::Voice(i, c);
        macro_rules! push {
            ($field:ident, $cmd:expr) => {
                if old.is_none_or(|o| o.$field != self.$field) {
                    out.push($cmd);
                }
            };
        }
        if old.is_none_or(|o| o.buffer != self.buffer) {
            out.push(EngineCmd::VoiceBuffer(i, self.buffer));
            for channel in 0..BUFFERS {
                let amount = if channel == self.buffer { 1.0 } else { 0.0 };
                out.push(EngineCmd::InputLevel {
                    channel,
                    voice: i,
                    amount,
                });
            }
        }
        push!(loop_start, v(LoopStart(self.loop_start)));
        push!(loop_end, v(LoopEnd(self.loop_end)));
        push!(loop_on, v(Loop(self.loop_on)));
        push!(fade_time, v(FadeTime(self.fade_time)));
        push!(rate_slew, v(RateSlewTime(self.rate_slew)));
        push!(rate, v(Rate(self.rate)));
        push!(rec_level, v(RecLevel(self.rec_level)));
        push!(pre_level, v(PreLevel(self.pre_level)));
        push!(post_fc, v(PostFilterFc(self.post_fc)));
        push!(post_rq, v(PostFilterRq(self.post_rq)));
        push!(post_lp, v(PostFilterLp(self.post_lp)));
        push!(post_hp, v(PostFilterHp(self.post_hp)));
        push!(post_bp, v(PostFilterBp(self.post_bp)));
        push!(post_dry, v(PostFilterDry(self.post_dry)));
        push!(rec_fade_shape, v(RecFadeShape(self.rec_fade_shape)));
        push!(pre_fade_shape, v(PreFadeShape(self.pre_fade_shape)));
        push!(rec_delay_ratio, v(RecDelayRatio(self.rec_delay_ratio)));
        push!(pre_window_ratio, v(PreWindowRatio(self.pre_window_ratio)));
        push!(level, EngineCmd::Level(i, self.level));
        push!(pan, EngineCmd::Pan(i, self.pan));
        push!(input_gain, EngineCmd::InputGain(i, self.input_gain));
        push!(play, v(Play(self.play)));
        push!(rec, v(Rec(self.rec)));
    }
}

/// SplitMix64: a small, fast generator, adequate for musical randomness.
/// The standard library has none.
struct Rng(u64);

impl Rng {
    fn from_time() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64);
        Self(nanos)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform in [0, 1).
    fn unit(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32
    }

    fn range(&mut self, lo: f32, hi: f32) -> f32 {
        lo + (hi - lo) * self.unit()
    }

    /// Uniform on a log scale.
    fn log_range(&mut self, lo: f32, hi: f32) -> f32 {
        (lo.ln() + (hi.ln() - lo.ln()) * self.unit()).exp()
    }

    fn pick<T: Copy>(&mut self, xs: &[T]) -> T {
        xs[(self.next_u64() % xs.len() as u64) as usize]
    }
}

/// Which settings randomization changes.
#[derive(Clone, Copy, PartialEq, Debug)]
struct Targets {
    rate: bool,
    loop_region: bool,
    pan_level: bool,
    filter: bool,
}

/// Octaves and fifths, so random rates stay in tune.
const RATES: [f32; 5] = [0.5, 0.75, 1.0, 1.5, 2.0];
const REVERSE_CHANCE: f32 = 0.3;
const MIN_LOOP: f32 = 0.05;

impl VoiceUi {
    /// New random values for the targeted settings; `content` is the seconds
    /// of material a loop may span.
    fn randomize(&mut self, rng: &mut Rng, t: Targets, content: f32) {
        if t.rate {
            let sign = if rng.unit() < REVERSE_CHANCE {
                -1.0
            } else {
                1.0
            };
            self.rate = sign * rng.pick(&RATES);
        }
        if t.loop_region {
            let len = (content * rng.range(0.05, 0.5)).max(MIN_LOOP).min(content);
            self.loop_start = rng.range(0.0, content - len);
            self.loop_end = self.loop_start + len;
        }
        if t.pan_level {
            self.pan = rng.range(-1.0, 1.0);
            self.level = rng.range(0.4, 0.9);
        }
        if t.filter {
            // A quarter of the time, no filter.
            let mode = rng.pick(&[0, 1, 2, 3]);
            (self.post_lp, self.post_bp, self.post_hp) = (0.0, 0.0, 0.0);
            self.post_dry = if mode == 0 { 1.0 } else { 0.0 };
            match mode {
                1 => self.post_lp = 1.0,
                2 => self.post_bp = 1.0,
                3 => self.post_hp = 1.0,
                _ => {}
            }
            self.post_fc = rng.log_range(200.0, 12000.0);
            self.post_rq = rng.log_range(0.3, 2.0);
        }
    }
}

/// Randomize voice `which`, or every voice. A linked pair gets one random
/// draw, mirrored onto the partner, so it stays a stereo pair.
fn randomize_voices(
    voices: &mut [VoiceUi; VOICES],
    linked: &[bool; VOICES / 2],
    which: Option<usize>,
    rng: &mut Rng,
    t: Targets,
    content: f32,
) {
    let chosen: Vec<usize> = match which {
        Some(i) => vec![i],
        None => (0..VOICES)
            .filter(|&i| i % 2 == 0 || !linked[i / 2])
            .collect(),
    };
    for i in chosen {
        voices[i].randomize(rng, t, content);
        if linked[i / 2] {
            voices[i ^ 1] = voices[i].partner();
        }
    }
}

/// Where a voice's loop crossfades happen, in buffer seconds: the incoming
/// head fades in from its entry point, the outgoing one fades out past its
/// exit. A crossfade spans `fade_time` of buffer whatever the rate, since
/// the fade advances with the head.
struct FadeZone {
    /// Where the fade starts.
    origin: f32,
    /// Direction of travel through the buffer: 1 forward, -1 reverse.
    dir: f32,
    fading_in: bool,
}

impl VoiceUi {
    /// A `Voice` with this voice's fade curves, for reading them on the UI
    /// thread; the real voice lives on the audio thread.
    fn shadow(&self, sample_rate: f32) -> Voice {
        let mut shadow = Voice::with_quirks(sample_rate, Quirks::Fixed);
        shadow.set_rec_fade_shape(self.rec_fade_shape);
        shadow.set_pre_fade_shape(self.pre_fade_shape);
        shadow.set_rec_delay_ratio(self.rec_delay_ratio);
        shadow.set_pre_window_ratio(self.pre_window_ratio);
        shadow
    }

    fn fade_zones(&self) -> Vec<FadeZone> {
        if self.fade_time <= 0.0 {
            return Vec::new();
        }
        // softcut treats rate 0 as reverse.
        let (entry, exit, dir) = match self.rate > 0.0 {
            true => (self.loop_start, self.loop_end, 1.0),
            false => (self.loop_end, self.loop_start, -1.0),
        };
        let mut zones = vec![FadeZone {
            origin: exit,
            dir,
            fading_in: false,
        }];
        if self.loop_on {
            zones.push(FadeZone {
                origin: entry,
                dir,
                fading_in: true,
            });
        }
        zones
    }
}

/// The equal-power output gain of a head at fade progress `fade`.
fn gain(fade: f32) -> f32 {
    (fade * std::f32::consts::FRAC_PI_2).sin()
}

impl FadeZone {
    /// Points (buffer seconds, fade progress) along the fade.
    fn points(&self, fade_time: f32) -> impl Iterator<Item = (f32, f32)> + '_ {
        const N: usize = 32;
        (0..=N).map(move |k| {
            let progress = k as f32 / N as f32;
            let fade = if self.fading_in {
                progress
            } else {
                1.0 - progress
            };
            (self.origin + self.dir * progress * fade_time, fade)
        })
    }
}

/// The voice's rec and pre curves and the heads' output gain against
/// crossfade progress, read from a shadow `Voice` with the same settings.
fn fade_curve_plot(ui: &mut egui::Ui, v: &VoiceUi, sample_rate: f32, color: Color32) {
    let shadow = v.shadow(sample_rate);

    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 90.0), Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 2.0, ui.visuals().extreme_bg_color);
    let plot = rect.shrink(6.0);
    let at = |x: f32, y: f32| {
        Pos2::new(
            plot.left() + x * plot.width(),
            plot.bottom() - y * plot.height(),
        )
    };
    let curve = |f: &dyn Fn(f32) -> f32| -> Vec<Pos2> {
        (0..=100)
            .map(|k| k as f32 / 100.0)
            .map(|x| at(x, f(x)))
            .collect()
    };
    let dim = ui.visuals().weak_text_color();
    let gain = |x: f32| (x * std::f32::consts::FRAC_PI_2).sin();
    painter.add(egui::Shape::line(curve(&gain), Stroke::new(1.0, dim)));
    painter.add(egui::Shape::line(
        curve(&|x| shadow.pre_fade_value(x)),
        Stroke::new(1.5, ui.visuals().text_color()),
    ));
    painter.add(egui::Shape::line(
        curve(&|x| shadow.rec_fade_value(x)),
        Stroke::new(2.0, color),
    ));
    let font = egui::FontId::monospace(10.0);
    painter.text(
        plot.left_top(),
        egui::Align2::LEFT_TOP,
        "rec",
        font.clone(),
        color,
    );
    painter.text(
        plot.left_top() + Vec2::new(28.0, 0.0),
        egui::Align2::LEFT_TOP,
        "pre",
        font.clone(),
        ui.visuals().text_color(),
    );
    painter.text(
        plot.left_top() + Vec2::new(56.0, 0.0),
        egui::Align2::LEFT_TOP,
        "gain",
        font.clone(),
        dim,
    );
    painter.text(
        plot.right_bottom(),
        egui::Align2::RIGHT_BOTTOM,
        "fade 0 -> 1",
        font,
        dim,
    );
}

/// Frames and buffers to save for a voice's loop: its region, in either
/// order, from both buffers for a linked stereo pair, else its own buffer.
fn loop_save_plan(v: &VoiceUi, linked: bool, sample_rate: f32) -> (usize, usize, Vec<usize>) {
    let frames = |t: f32| (t.max(0.0) * sample_rate).round() as usize;
    let (a, b) = (frames(v.loop_start), frames(v.loop_end));
    let buffers = if linked { vec![0, 1] } else { vec![v.buffer] };
    (a.min(b), a.max(b), buffers)
}

/// The UI's copy of an effects chain's settings; defaults match
/// `softcut_fx::Chain::new`, with every effect off.
#[derive(Clone, Copy, PartialEq, Debug)]
struct FxUi {
    on: [bool; 5],
    drive: f32,
    level: f32,
    bits: f32,
    downsample: f32,
    crush_mix: f32,
    chorus_rate: f32,
    chorus_depth: f32,
    chorus_mix: f32,
    delay_time: f32,
    delay_feedback: f32,
    delay_damp: f32,
    delay_mix: f32,
    reverb_size: f32,
    reverb_damp: f32,
    reverb_width: f32,
    reverb_mix: f32,
}

impl Default for FxUi {
    fn default() -> Self {
        Self {
            on: [false; 5],
            drive: 6.0,
            level: -3.0,
            bits: 8.0,
            downsample: 4.0,
            crush_mix: 1.0,
            chorus_rate: 0.5,
            chorus_depth: 3.0,
            chorus_mix: 0.5,
            delay_time: 0.375,
            delay_feedback: 0.4,
            delay_damp: 0.3,
            delay_mix: 0.35,
            reverb_size: 0.7,
            reverb_damp: 0.5,
            reverb_width: 1.0,
            reverb_mix: 0.3,
        }
    }
}

impl FxUi {
    /// Commands that take a chain from `old` to `self`.
    fn diff(&self, old: &Self) -> Vec<FxCmd> {
        use FxCmd::*;
        let mut out = Vec::new();
        for fx in Fx::ALL {
            let k = fx as usize;
            if self.on[k] != old.on[k] {
                out.push(Enable(fx, self.on[k]));
            }
        }
        // (new value, old value, command setting it)
        type Setting = (f32, f32, fn(f32) -> FxCmd);
        let pairs: [Setting; 16] = [
            (self.drive, old.drive, Drive),
            (self.level, old.level, Level),
            (self.bits, old.bits, Bits),
            (self.downsample, old.downsample, Downsample),
            (self.crush_mix, old.crush_mix, CrushMix),
            (self.chorus_rate, old.chorus_rate, ChorusRate),
            (self.chorus_depth, old.chorus_depth, ChorusDepth),
            (self.chorus_mix, old.chorus_mix, ChorusMix),
            (self.delay_time, old.delay_time, DelayTime),
            (self.delay_feedback, old.delay_feedback, DelayFeedback),
            (self.delay_damp, old.delay_damp, DelayDamp),
            (self.delay_mix, old.delay_mix, DelayMix),
            (self.reverb_size, old.reverb_size, ReverbSize),
            (self.reverb_damp, old.reverb_damp, ReverbDamp),
            (self.reverb_width, old.reverb_width, ReverbWidth),
            (self.reverb_mix, old.reverb_mix, ReverbMix),
        ];
        out.extend(
            pairs
                .into_iter()
                .filter(|(a, b, _)| a != b)
                .map(|(a, _, cmd)| cmd(a)),
        );
        out
    }
}

/// Controls for one effects chain, in processing order.
fn fx_controls(ui: &mut egui::Ui, fx: &mut FxUi, id: &str) {
    use egui::Slider;
    let section =
        |ui: &mut egui::Ui, on: &mut bool, name: &str, body: &mut dyn FnMut(&mut egui::Ui)| {
            ui.checkbox(on, name);
            if *on {
                ui.indent((id, name), |ui| body(ui));
            }
        };
    let [sat, crush, chorus, delay, reverb] = &mut fx.on;
    section(ui, sat, "saturation", &mut |ui| {
        ui.add(
            Slider::new(&mut fx.drive, 0.0..=36.0)
                .text("drive")
                .suffix(" dB"),
        );
        ui.add(
            Slider::new(&mut fx.level, -24.0..=6.0)
                .text("level")
                .suffix(" dB"),
        );
    });
    section(ui, crush, "bitcrusher", &mut |ui| {
        ui.add(Slider::new(&mut fx.bits, 1.0..=16.0).text("bits"));
        ui.add(
            Slider::new(&mut fx.downsample, 1.0..=32.0)
                .logarithmic(true)
                .text("downsample"),
        );
        ui.add(Slider::new(&mut fx.crush_mix, 0.0..=1.0).text("mix"));
    });
    section(ui, chorus, "chorus", &mut |ui| {
        ui.add(
            Slider::new(&mut fx.chorus_rate, 0.05..=5.0)
                .logarithmic(true)
                .text("rate")
                .suffix(" Hz"),
        );
        ui.add(
            Slider::new(&mut fx.chorus_depth, 0.0..=10.0)
                .text("depth")
                .suffix(" ms"),
        );
        ui.add(Slider::new(&mut fx.chorus_mix, 0.0..=1.0).text("mix"));
    });
    section(ui, delay, "delay", &mut |ui| {
        ui.add(
            Slider::new(&mut fx.delay_time, 0.01..=2.0)
                .logarithmic(true)
                .text("time")
                .suffix(" s"),
        );
        ui.add(Slider::new(&mut fx.delay_feedback, 0.0..=0.95).text("feedback"));
        ui.add(Slider::new(&mut fx.delay_damp, 0.0..=1.0).text("damping"));
        ui.add(Slider::new(&mut fx.delay_mix, 0.0..=1.0).text("mix"));
    });
    section(ui, reverb, "reverb", &mut |ui| {
        ui.add(Slider::new(&mut fx.reverb_size, 0.0..=1.0).text("size"));
        ui.add(Slider::new(&mut fx.reverb_damp, 0.0..=1.0).text("damping"));
        ui.add(Slider::new(&mut fx.reverb_width, 0.0..=1.0).text("width"));
        ui.add(Slider::new(&mut fx.reverb_mix, 0.0..=1.0).text("mix"));
    });
}

/// A save waiting for both buffers' snapshots to come back.
struct PendingSave {
    path: PathBuf,
    /// First frame to write; snapshots run from 0 to the region's end.
    start: usize,
    /// The buffers to write, one per file channel.
    buffers: Vec<usize>,
    parts: [Option<Box<[f32]>>; BUFFERS],
}

struct App {
    audio: Audio,
    voices: [VoiceUi; VOICES],
    /// Per pair (voices 1+2, 3+4): edits to one voice apply to both.
    linked: [bool; VOICES / 2],
    feedback: [[f32; VOICES]; VOICES],
    selected: usize,
    source: Source,
    drag_from: Option<f32>,
    /// Result of the last load, save or failure.
    status: String,
    /// Displayed input level, linear, with meter ballistics applied.
    meter: f32,
    /// Length of the loaded sample, if one is loaded.
    sample_seconds: Option<f32>,
    /// Seconds from the buffer start shown on the waveform.
    view_len: f32,
    saving: Option<PendingSave>,
    /// While the input is on: since when it has delivered no signal.
    stall: Option<Stall>,
    rng: Rng,
    targets: Targets,
    /// Re-randomize all voices every `auto_seconds`, when on.
    auto_random: bool,
    auto_seconds: f32,
    last_random: f64,
    osc: Option<softcut_osc::Server>,
    osc_port: u16,
    /// Effects: a mono insert per voice, and a stereo chain on the mix.
    voice_fx: [FxUi; VOICES],
    master_fx: FxUi,
    phase_poll: PhasePoll,
}

/// Input counters as of the last time the input carried signal.
struct Stall {
    since: f64,
    frames: u64,
    signal: u64,
    reported: bool,
}

impl App {
    /// Seconds of meaningful content: the loaded sample, else the whole buffer.
    fn content_seconds(&self) -> f32 {
        self.sample_seconds.unwrap_or(self.audio.buffer_seconds)
    }

    fn set_view_len(&mut self, len: f32) {
        self.view_len = len.clamp(0.01, self.audio.buffer_seconds);
        let frames = (self.view_len * self.audio.sample_rate) as usize;
        self.audio.meters.set_view_frames(frames);
    }

    /// Voice `i`, and its partner if the pair is linked.
    fn targets(&self, i: usize) -> Vec<usize> {
        if self.linked[i / 2] {
            vec![i, i ^ 1]
        } else {
            vec![i]
        }
    }

    /// Input level in dBFS over [-48, 0]: instant rise, ~300 ms fall.
    fn input_meter(&mut self, ui: &mut egui::Ui) {
        const FLOOR_DB: f32 = -48.0;
        let peak = self.audio.meters.take_input_peak();
        let dt = ui.input(|i| i.stable_dt);
        self.meter = peak.max(self.meter * (-dt / 0.3).exp());
        let db = 20.0 * self.meter.max(1e-6).log10();
        let fill = ((db - FLOOR_DB) / -FLOOR_DB).clamp(0.0, 1.0);

        ui.label("level");
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(120.0, 8.0), Sense::hover());
        let painter = ui.painter();
        painter.rect_filled(rect, 0.0, ui.visuals().extreme_bg_color);
        let mut bar = rect;
        bar.set_width(rect.width() * fill);
        let color = if db > -1.0 {
            Color32::from_rgb(230, 90, 80)
        } else {
            Color32::from_rgb(120, 200, 110)
        };
        painter.rect_filled(bar, 0.0, color);
        resp.on_hover_text(format!("input peak {db:.1} dBFS"));
    }

    /// Load a WAV into both buffers, stop recording, and loop every voice over it.
    fn load_wav(&mut self, path: &Path) {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let loaded = match wav::load(path, self.audio.sample_rate, BUFFER_FRAMES) {
            Ok(l) => l,
            Err(e) => {
                self.status = format!("{name}: {e}");
                return;
            }
        };
        let (seconds, truncated, rate) = (loaded.seconds, loaded.truncated, loaded.source_rate);
        for (b, data) in loaded.data.into_iter().enumerate() {
            if let Err(e) = self.audio.handle.load(b, data) {
                self.status = format!("{name}: {e}");
                return;
            }
        }
        // Loop settings go out with this frame's diff; the cuts go now. Both
        // apply after the load.
        for i in 0..VOICES {
            let v = &mut self.voices[i];
            v.rec = false;
            v.loop_start = 0.0;
            v.loop_end = seconds.max(0.01);
            self.send(EngineCmd::Voice(i, VoiceCmd::CutTo(0.0)));
        }
        self.sample_seconds = Some(seconds.max(0.01));
        self.set_view_len(seconds);
        self.status = format!("{name}: {seconds:.1} s");
        if rate as f32 != self.audio.sample_rate {
            self.status += &format!(", resampled from {rate} Hz");
        }
        if truncated {
            self.status += ", truncated to fit the buffer";
        }
    }

    /// Snapshot both buffers over the content length; `collect_returned`
    /// writes the file once both copies are back.
    /// Snapshot `buffers` up to frame `end`; `collect_returned` writes frames
    /// `start..end` as one file channel per buffer once all are back.
    fn start_save(&mut self, path: PathBuf, start: usize, end: usize, buffers: &[usize]) {
        if self.saving.is_some() {
            self.status = "a save is already in progress".into();
            return;
        }
        let end = end.min(BUFFER_FRAMES);
        if start >= end || buffers.iter().any(|&b| b >= BUFFERS) {
            self.status = "save: empty region or no such buffer".into();
            return;
        }
        for &b in buffers {
            if let Err(e) = self.audio.handle.snapshot(b, vec![0.0; end].into()) {
                self.status = format!("save: {e}");
                return;
            }
        }
        self.saving = Some(PendingSave {
            path,
            start,
            buffers: buffers.to_vec(),
            parts: [None, None],
        });
    }

    /// Take buffers back from the audio thread: snapshots feed a pending save,
    /// the rest are freed here rather than on the audio thread.
    fn collect_returned(&mut self) {
        while let Some(r) = self.audio.handle.returned() {
            if let Returned::Snapshot { buffer, data } = r
                && let Some(save) = &mut self.saving
            {
                save.parts[buffer] = Some(data);
            }
        }
        let done = self
            .saving
            .as_ref()
            .is_some_and(|s| s.buffers.iter().all(|&b| s.parts[b].is_some()));
        if !done {
            return;
        }
        let save = self.saving.take().unwrap();
        let channels: Vec<&[f32]> = save
            .buffers
            .iter()
            .map(|&b| &save.parts[b].as_ref().unwrap()[save.start..])
            .collect();
        let name = save
            .path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let sr = self.audio.sample_rate;
        self.status = match wav::save(&save.path, &channels, sr as u32) {
            Ok(()) => format!("saved {name}: {:.1} s", channels[0].len() as f32 / sr),
            Err(e) => format!("save {name}: {e}"),
        };
    }

    fn send(&mut self, cmd: EngineCmd) {
        // A full ring means the audio thread has stalled; the handle counts the
        // drop, and blocking the UI would not help.
        let _ = self.audio.handle.send(cmd);
    }

    fn waveform(&mut self, ui: &mut egui::Ui) {
        let size = Vec2::new(ui.available_width(), 260.0);
        let (resp, painter) = ui.allocate_painter(size, Sense::click_and_drag());
        let rect = resp.rect;
        let len = self.view_len;
        let x_of = |t: f32| rect.left() + t / len * rect.width();
        let t_of = |x: f32| ((x - rect.left()) / rect.width() * len).clamp(0.0, len);
        let lane_h = (rect.height() - 4.0) / BUFFERS as f32;
        let wave = ui.visuals().text_color();
        let dim = ui.visuals().weak_text_color();
        let label = egui::FontId::monospace(11.0);
        let bin_w = rect.width() / WAVE_BINS as f32;
        let shadow = self.voices[self.selected].shadow(self.audio.sample_rate);

        for b in 0..BUFFERS {
            let top = rect.top() + b as f32 * (lane_h + 4.0);
            let lane = Rect::from_x_y_ranges(rect.x_range(), top..=top + lane_h);
            painter.rect_filled(lane, 4.0, ui.visuals().extreme_bg_color);
            for (i, v) in self.voices.iter().enumerate() {
                if v.buffer != b {
                    continue;
                }
                let band =
                    Rect::from_x_y_ranges(x_of(v.loop_start)..=x_of(v.loop_end), lane.y_range());
                let alpha = if i == self.selected { 60 } else { 22 };
                painter.rect_filled(band, 0.0, COLORS[i].gamma_multiply_u8(alpha));
                // Across each crossfade, from 0 at the lane's bottom to 1 near
                // its top: playback gain for every voice; for the selected
                // voice also the levels it records with, which the fade
                // curves shape.
                let y = |level: f32| lane.bottom() - level * lane_h * 0.9;
                let (width, alpha) = if i == self.selected {
                    (2.0, 255)
                } else {
                    (1.0, 110)
                };
                for zone in v.fade_zones() {
                    let line = |f: &dyn Fn(f32) -> f32| -> Vec<Pos2> {
                        zone.points(v.fade_time)
                            .map(|(t, fade)| Pos2::new(x_of(t), y(f(fade))))
                            .collect()
                    };
                    painter.add(egui::Shape::line(
                        line(&gain),
                        Stroke::new(width, COLORS[i].gamma_multiply_u8(alpha)),
                    ));
                    // The rec and pre curves only shape what a crossfade records.
                    if i == self.selected && v.rec {
                        let rec = line(&|fade| v.rec_level * shadow.rec_fade_value(fade));
                        painter.extend(egui::Shape::dashed_line(
                            &rec,
                            Stroke::new(1.5, COLORS[i]),
                            4.0,
                            3.0,
                        ));
                        let pre = line(&|fade| {
                            v.pre_level + (1.0 - v.pre_level) * shadow.pre_fade_value(fade)
                        });
                        painter.add(egui::Shape::line(pre, Stroke::new(1.5, dim)));
                    }
                }
            }
            let mid = lane.center().y;
            for bin in 0..WAVE_BINS {
                let h = self.audio.meters.peak(b, bin).min(1.0) * lane_h * 0.5;
                let x = rect.left() + (bin as f32 + 0.5) * bin_w;
                painter.line_segment(
                    [Pos2::new(x, mid - h), Pos2::new(x, mid + h)],
                    Stroke::new(bin_w.max(1.0), wave),
                );
            }
            for i in 0..VOICES {
                let v = &self.voices[i];
                if v.buffer != b || (!v.play && !v.rec) {
                    continue;
                }
                // Both heads, each as bright as its gain: during a crossfade
                // one fades out as the other fades in.
                let width = if v.rec { 3.0 } else { 1.5 };
                for h in self.audio.handle.heads(i) {
                    if h.gain < 0.01 || !(0.0..=len).contains(&h.position) {
                        continue;
                    }
                    let color = COLORS[i].gamma_multiply(h.gain);
                    painter.vline(x_of(h.position), lane.y_range(), Stroke::new(width, color));
                }
            }
            painter.text(
                lane.left_top() + Vec2::new(4.0, 2.0),
                egui::Align2::LEFT_TOP,
                ["L", "R"][b],
                label.clone(),
                dim,
            );
        }
        painter.text(
            rect.left_bottom() + Vec2::new(4.0, -2.0),
            egui::Align2::LEFT_BOTTOM,
            "0 s",
            label.clone(),
            dim,
        );
        painter.text(
            rect.right_bottom() + Vec2::new(-4.0, -2.0),
            egui::Align2::RIGHT_BOTTOM,
            format!("{len:.2} s"),
            label.clone(),
            dim,
        );
        painter.text(
            rect.right_top() + Vec2::new(-4.0, 2.0),
            egui::Align2::RIGHT_TOP,
            "crossfades: solid = playback gain; while recording, dashed = rec level, grey = kept level",
            label,
            dim,
        );

        let sel = self.selected;
        if let Some(pos) = resp.interact_pointer_pos() {
            let t = t_of(pos.x);
            if resp.drag_started() {
                self.drag_from = Some(t);
            }
            if resp.dragged()
                && let Some(t0) = self.drag_from
                && (t - t0).abs() > 0.01
            {
                self.voices[sel].loop_start = t0.min(t);
                self.voices[sel].loop_end = t0.max(t);
            }
            if resp.clicked() {
                for i in self.targets(sel) {
                    self.send(EngineCmd::Voice(i, VoiceCmd::CutTo(t)));
                }
            }
        }
        if resp.drag_stopped() {
            self.drag_from = None;
        }
    }

    fn voice_controls(&mut self, ui: &mut egui::Ui) {
        let secs = self.content_seconds();
        let i = self.selected;
        let pair = i / 2;
        let was_linked = self.linked[pair];
        ui.horizontal(|ui| {
            ui.checkbox(
                &mut self.linked[pair],
                format!("link voices {} + {}", pair * 2 + 1, pair * 2 + 2),
            )
            .on_hover_text(
                "stereo pair: edits apply to both, on opposite buffers with mirrored pan",
            );
        });
        if self.linked[pair] && !was_linked {
            self.voices[i ^ 1] = self.voices[i].partner();
        }
        let sr = self.audio.sample_rate;
        let v = &mut self.voices[i];
        ui.horizontal(|ui| {
            ui.toggle_value(&mut v.play, "play");
            ui.toggle_value(&mut v.rec, "rec");
            ui.checkbox(&mut v.loop_on, "loop");
            ui.label("buffer");
            ui.selectable_value(&mut v.buffer, 0, "L");
            ui.selectable_value(&mut v.buffer, 1, "R");
        });
        let (mut cut, mut rec_once, mut clear, mut reverse) = (false, false, false, false);
        ui.horizontal(|ui| {
            cut = ui.button("cut to start").clicked();
            rec_once = ui
                .button("rec once")
                .on_hover_text("record one pass of the loop")
                .clicked();
            clear = ui.button("clear loop").clicked();
            reverse = ui
                .button("reverse loop")
                .on_hover_text("reverse the loop region of the buffer in place")
                .clicked();
        });
        egui::Grid::new("voice").num_columns(2).show(ui, |ui| {
            let row = |ui: &mut egui::Ui, label: &str, w: egui::Slider| {
                ui.label(label);
                ui.add(w);
                ui.end_row();
            };
            row(ui, "rate", egui::Slider::new(&mut v.rate, -4.0..=4.0));
            row(
                ui,
                "loop start",
                egui::Slider::new(&mut v.loop_start, 0.0..=secs)
                    .suffix(" s")
                    .clamping(egui::SliderClamping::Edits),
            );
            row(
                ui,
                "loop end",
                egui::Slider::new(&mut v.loop_end, 0.0..=secs)
                    .suffix(" s")
                    .clamping(egui::SliderClamping::Edits),
            );
            row(
                ui,
                "fade time",
                egui::Slider::new(&mut v.fade_time, 0.0..=10.0)
                    .logarithmic(true)
                    .suffix(" s"),
            );
            row(
                ui,
                "rate slew",
                egui::Slider::new(&mut v.rate_slew, 0.0..=4.0).suffix(" s"),
            );
            row(
                ui,
                "rec level",
                egui::Slider::new(&mut v.rec_level, 0.0..=1.0),
            );
            row(
                ui,
                "pre level",
                egui::Slider::new(&mut v.pre_level, 0.0..=1.0),
            );
            row(ui, "level", egui::Slider::new(&mut v.level, 0.0..=1.0));
            row(ui, "pan", egui::Slider::new(&mut v.pan, -1.0..=1.0));
            row(
                ui,
                "input gain",
                egui::Slider::new(&mut v.input_gain, 0.0..=1.0),
            );
            row(
                ui,
                "post fc",
                egui::Slider::new(&mut v.post_fc, 20.0..=16000.0)
                    .logarithmic(true)
                    .suffix(" Hz"),
            );
            row(
                ui,
                "post rq",
                egui::Slider::new(&mut v.post_rq, 0.05..=4.0).logarithmic(true),
            );
            row(ui, "post lp", egui::Slider::new(&mut v.post_lp, 0.0..=1.0));
            row(ui, "post hp", egui::Slider::new(&mut v.post_hp, 0.0..=1.0));
            row(ui, "post bp", egui::Slider::new(&mut v.post_bp, 0.0..=1.0));
            row(
                ui,
                "post dry",
                egui::Slider::new(&mut v.post_dry, 0.0..=1.0),
            );
        });
        egui::CollapsingHeader::new("crossfade curves")
            .id_salt("curves")
            .show(ui, |ui| {
                let shapes = [
                    (FadeShape::Linear, "linear"),
                    (FadeShape::Sine, "sine"),
                    (FadeShape::Raised, "raised"),
                ];
                ui.horizontal(|ui| {
                    ui.label("rec shape")
                        .on_hover_text("how new input fades in across a crossfade");
                    for (shape, name) in shapes {
                        ui.selectable_value(&mut v.rec_fade_shape, shape, name);
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("pre shape")
                        .on_hover_text("how existing content is kept across a crossfade");
                    for (shape, name) in shapes {
                        ui.selectable_value(&mut v.pre_fade_shape, shape, name);
                    }
                });
                ui.add(egui::Slider::new(&mut v.rec_delay_ratio, 0.0..=1.0).text("rec delay"))
                    .on_hover_text("fraction of the crossfade before new input fades in");
                ui.add(egui::Slider::new(&mut v.pre_window_ratio, 0.0..=1.0).text("pre window"))
                    .on_hover_text("fraction of the crossfade over which existing content is kept");
                fade_curve_plot(ui, v, sr, COLORS[i]);
            });
        egui::CollapsingHeader::new("effects")
            .id_salt("voice fx")
            .show(ui, |ui| {
                ui.label("mono, before pan; feedback to other voices carries it");
                fx_controls(ui, &mut self.voice_fx[i], "voice fx");
            });

        for t in self.targets(i) {
            let v = self.voices[t];
            let region = (v.loop_start, v.loop_end - v.loop_start);
            if cut {
                self.send(EngineCmd::Voice(t, VoiceCmd::CutTo(v.loop_start)));
            }
            if rec_once {
                self.send(EngineCmd::Voice(t, VoiceCmd::RecOnce(true)));
            }
            if clear {
                self.send(EngineCmd::ClearRegion {
                    buffer: v.buffer,
                    start: region.0,
                    len: region.1,
                    fade: EDIT_FADE,
                    preserve: 0.0,
                });
            }
            if reverse {
                self.send(EngineCmd::CopyRegion {
                    src: v.buffer,
                    dst: v.buffer,
                    src_start: region.0,
                    dst_start: region.0,
                    len: region.1,
                    fade: EDIT_FADE,
                    preserve: 0.0,
                    reverse: true,
                });
            }
        }
    }

    fn osc_row(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            let mut on = self.osc.is_some();
            if ui
                .checkbox(&mut on, "OSC")
                .on_hover_text("softcut_jack_osc protocol, 0-based indices, loopback only")
                .changed()
            {
                self.osc = None;
                self.phase_poll.observe(&Action::PhasePoll(false));
                if on {
                    let reply = softcut_osc::DEFAULT_REPLY.parse().expect("valid address");
                    match softcut_osc::Server::start(("127.0.0.1", self.osc_port), reply) {
                        Ok(server) => self.osc = Some(server),
                        Err(e) => self.status = format!("OSC: {e}"),
                    }
                }
            }
            ui.add_enabled(
                self.osc.is_none(),
                egui::DragValue::new(&mut self.osc_port)
                    .range(1024..=65535)
                    .prefix("port "),
            );
            if let Some(server) = &self.osc {
                let at = server.local_addr().map_or("?".into(), |a| a.to_string());
                ui.label(format!(
                    "listening on {at}, polls to {}",
                    softcut_osc::DEFAULT_REPLY
                ));
                if server.dropped() > 0 {
                    ui.label(format!("{} dropped", server.dropped()));
                }
            }
        });
    }

    /// Apply everything the OSC server has received, and send phase polls.
    fn drain_osc(&mut self) {
        let Some(server) = &self.osc else { return };
        let items: Vec<_> = std::iter::from_fn(|| server.try_recv()).collect();
        for item in items {
            match item {
                Ok(action) => {
                    self.phase_poll.observe(&action);
                    self.apply_osc(action);
                }
                Err(e) => self.status = format!("OSC: {e}"),
            }
        }
        if let Some(server) = &self.osc {
            let handle = &self.audio.handle;
            for (v, phase) in self.phase_poll.changed(|v| handle.position(v)) {
                let _ = server.send_phase(v, phase);
            }
        }
    }

    fn apply_osc(&mut self, action: Action) {
        let sr = self.audio.sample_rate;
        let frames = |t: f32| (t.max(0.0) * sr) as usize;
        match action {
            Action::Engine(cmd) => {
                match cmd {
                    EngineCmd::Voice(i, c) if i < VOICES => self.voices[i].mirror(&c),
                    EngineCmd::Level(i, x) if i < VOICES => self.voices[i].level = x,
                    EngineCmd::Pan(i, x) if i < VOICES => self.voices[i].pan = x,
                    EngineCmd::InputGain(i, x) if i < VOICES => self.voices[i].input_gain = x,
                    EngineCmd::VoiceBuffer(i, b) if i < VOICES && b < BUFFERS => {
                        self.voices[i].buffer = b
                    }
                    EngineCmd::Feedback { src, dst, amount } if src < VOICES && dst < VOICES => {
                        self.feedback[src][dst] = amount
                    }
                    _ => {}
                }
                self.send(cmd);
            }
            Action::ReadMono {
                path,
                start_src,
                start_dst,
                dur,
                ch_src,
                ch_dst,
            } => self.osc_read(&path, start_src, start_dst, dur, &[(ch_src.min(1), ch_dst)]),
            Action::ReadStereo {
                path,
                start_src,
                start_dst,
                dur,
            } => self.osc_read(&path, start_src, start_dst, dur, &[(0, 0), (1, 1)]),
            Action::WriteMono {
                path,
                start,
                dur,
                ch,
            } => {
                let end = if dur < 0.0 {
                    BUFFER_FRAMES
                } else {
                    frames(start + dur)
                };
                self.start_save(path, frames(start), end, &[ch]);
            }
            Action::WriteStereo { path, start, dur } => {
                let end = if dur < 0.0 {
                    BUFFER_FRAMES
                } else {
                    frames(start + dur)
                };
                self.start_save(path, frames(start), end, &[0, 1]);
            }
            Action::Reset => {
                // Back to the demo's startup state, rather than the
                // reference's silent, disabled voices.
                for i in 0..VOICES {
                    self.send(EngineCmd::Voice(i, VoiceCmd::Reset));
                }
                self.voices = std::array::from_fn(VoiceUi::preset);
                let mut cmds = Vec::new();
                for (i, v) in self.voices.iter().enumerate() {
                    v.diff(i, None, &mut cmds);
                }
                for (src, dst) in (0..VOICES).flat_map(|s| (0..VOICES).map(move |d| (s, d))) {
                    self.feedback[src][dst] = 0.0;
                    cmds.push(EngineCmd::Feedback {
                        src,
                        dst,
                        amount: 0.0,
                    });
                }
                cmds.into_iter().for_each(|c| self.send(c));
            }
            Action::PhasePoll(_) | Action::Ignored(_) => {}
        }
    }

    /// Read file channels into buffers: `pairs` of (file channel, buffer).
    /// Unlike the reference, which reads at the file's rate, the file is
    /// resampled to the engine's.
    fn osc_read(
        &mut self,
        path: &Path,
        start_src: f32,
        start_dst: f32,
        dur: f32,
        pairs: &[(usize, usize)],
    ) {
        let sr = self.audio.sample_rate;
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let loaded = match wav::load(path, sr, BUFFER_FRAMES) {
            Ok(l) => l,
            Err(e) => {
                self.status = format!("OSC read {name}: {e}");
                return;
            }
        };
        let content = (loaded.seconds * sr) as usize;
        let s0 = ((start_src.max(0.0) * sr) as usize).min(content);
        let s1 = if dur < 0.0 {
            content
        } else {
            (s0 + (dur * sr) as usize).min(content)
        };
        for &(src, dst) in pairs {
            let data: Box<[f32]> = loaded.data[src][s0..s1].into();
            if let Err(e) = self.audio.handle.write(dst, start_dst, data, 0.0, 1.0, 0.0) {
                self.status = format!("OSC read {name}: {e}");
                return;
            }
        }
        self.status = format!("OSC read {name}: {:.1} s", (s1 - s0) as f32 / sr);
    }

    fn randomize(&mut self, which: Option<usize>, now: f64) {
        let content = self.content_seconds();
        randomize_voices(
            &mut self.voices,
            &self.linked,
            which,
            &mut self.rng,
            self.targets,
            content,
        );
        self.last_random = now;
    }

    /// Randomize targets, the selected voice or all, and a timer.
    fn random_row(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.label("randomize");
            let t = &mut self.targets;
            ui.checkbox(&mut t.rate, "rate")
                .on_hover_text("octaves and fifths, sometimes reversed");
            ui.checkbox(&mut t.loop_region, "loop");
            ui.checkbox(&mut t.pan_level, "pan/level");
            ui.checkbox(&mut t.filter, "filter");
            let now = ui.input(|i| i.time);
            if ui
                .button(format!("voice {}", self.selected + 1))
                .on_hover_text("the selected voice, and its partner if linked")
                .clicked()
            {
                self.randomize(Some(self.selected), now);
            }
            if ui.button("all").clicked() {
                self.randomize(None, now);
            }
            ui.separator();
            if ui.checkbox(&mut self.auto_random, "auto every").changed() && self.auto_random {
                self.last_random = now;
            }
            ui.add(
                egui::DragValue::new(&mut self.auto_seconds)
                    .range(0.25..=60.0)
                    .speed(0.05)
                    .suffix(" s"),
            );
        });
    }

    /// Start or stop recording the output mix.
    fn record_button(&mut self, ui: &mut egui::Ui) {
        let Some(secs) = self.audio.recorded_seconds() else {
            if ui
                .button("record output...")
                .on_hover_text("record what you hear to a stereo 32-bit float WAV")
                .clicked()
                && let Some(path) = rfd::FileDialog::new()
                    .add_filter("WAV", &["wav"])
                    .set_file_name("softcut-recording.wav")
                    .save_file()
            {
                self.status = match self.audio.start_recording(&path) {
                    Ok(()) => "recording the output".into(),
                    Err(e) => format!("record: {e}"),
                };
            }
            return;
        };
        let text = egui::RichText::new(format!("stop recording ({secs:.1} s)"))
            .color(Color32::from_rgb(230, 90, 80));
        if ui.button(text).clicked() {
            self.status = match self.audio.stop_recording() {
                Ok((path, secs)) => {
                    let name = path.file_name().unwrap_or_default().to_string_lossy();
                    format!("recorded {name}: {secs:.1} s")
                }
                Err(e) => format!("record: {e}"),
            };
        }
        let dropped = self.audio.meters.rec_dropped();
        if dropped > 0 {
            ui.label(format!("{} frames dropped", dropped / 2))
                .on_hover_text("the disk writer fell behind");
        }
    }

    /// Input device, channel pair, on/off and level.
    fn input_row(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.label("input");
            let current = self.audio.input_device();
            let label = |d: &InputDevice| match (d.loopback, d.is_default) {
                (true, _) => format!("system audio: {}", d.name),
                (false, true) => format!("{} (default)", d.name),
                (false, false) => d.name.clone(),
            };
            let mut device_pick = None;
            egui::ComboBox::from_id_salt("input device")
                .width(240.0)
                .selected_text(current.map_or("none".into(), |i| label(&self.audio.inputs[i])))
                .show_ui(ui, |ui| {
                    for (i, d) in self.audio.inputs.iter().enumerate() {
                        if ui.selectable_label(current == Some(i), label(d)).clicked() {
                            device_pick = Some(i);
                        }
                    }
                });
            let (first, channels) = self.audio.input_channels();
            let mut channel_pick = None;
            if channels > 2 {
                let pair = |f: usize| match f + 1 < channels {
                    true => format!("ch {}+{}", f + 1, f + 2),
                    false => format!("ch {}", f + 1),
                };
                egui::ComboBox::from_id_salt("input channels")
                    .selected_text(pair(first))
                    .show_ui(ui, |ui| {
                        for f in (0..channels).step_by(2) {
                            if ui.selectable_label(f == first, pair(f)).clicked() {
                                channel_pick = Some(f);
                            }
                        }
                    });
            }
            if ui
                .button("refresh")
                .on_hover_text("list devices again, e.g. after plugging one in")
                .clicked()
            {
                self.audio.refresh_devices();
            }
            let switch = match (device_pick, channel_pick) {
                (Some(d), _) => Some((d, 0)),
                (None, Some(f)) => current.map(|d| (d, f)),
                _ => None,
            };
            if let Some((device, first)) = switch {
                self.status = self.switch_input(device, first);
            }

            let old = self.source;
            ui.add_enabled_ui(self.audio.input_device().is_some(), |ui| {
                ui.selectable_value(&mut self.source, Source::On, "on")
                    .on_disabled_hover_text(self.audio.input_error.clone().unwrap_or_default());
            });
            ui.selectable_value(&mut self.source, Source::Off, "off");
            if self.source != old
                && let Err(e) = self.audio.set_source(self.source)
            {
                self.status = format!("input: {e}");
                self.source = old;
            }
            self.input_meter(ui);

            ui.separator();
            ui.label("output");
            let current = self.audio.output_device();
            let mut pick = None;
            egui::ComboBox::from_id_salt("output device")
                .width(200.0)
                .selected_text(self.audio.output_name())
                .show_ui(ui, |ui| {
                    for (i, d) in self.audio.outputs.iter().enumerate() {
                        let text = match d.is_default {
                            true => format!("{} (default)", d.name),
                            false => d.name.clone(),
                        };
                        if ui.selectable_label(current == Some(i), text).clicked() {
                            pick = Some(i);
                        }
                    }
                });
            if let Some(i) = pick {
                let name = self.audio.outputs[i].name.clone();
                self.status = match self.audio.select_output(i) {
                    Err(e) => format!("{name}: {e}"),
                    Ok(()) if self.audio.output_rate() as f32 != self.audio.sample_rate => {
                        format!(
                            "output: {name}, resampled to {} Hz",
                            self.audio.output_rate()
                        )
                    }
                    Ok(()) => format!("output: {name}"),
                };
            }
        });
    }

    /// Open an input and describe the result for the status line.
    fn switch_input(&mut self, device: usize, first: usize) -> String {
        let name = self.audio.inputs[device].name.clone();
        if let Err(e) = self.audio.select_input(device, first) {
            return format!("{name}: {e}");
        }
        self.stall = None;
        let mut status = format!("input: {name}");
        if let Some(rate) = self.audio.input_rate()
            && rate as f32 != self.audio.sample_rate
        {
            status += &format!(", resampled from {rate} Hz");
        }
        if self.audio.inputs[device].loopback {
            status += ". Captures everything playing on this device, softcut included: \
                       recording it while softcut plays feeds back";
        }
        status
    }

    /// While the input is on, report once if it has delivered nothing, or
    /// only exact silence, for 2 s.
    fn check_input_arriving(&mut self, now: f64) {
        if self.source != Source::On {
            self.stall = None;
            return;
        }
        let (frames, signal) = (
            self.audio.meters.input_frames(),
            self.audio.meters.input_signal(),
        );
        let st = match &mut self.stall {
            Some(st) if st.signal == signal => st,
            _ => {
                self.stall = Some(Stall {
                    since: now,
                    frames,
                    signal,
                    reported: false,
                });
                return;
            }
        };
        if st.reported || now - st.since < 2.0 {
            return;
        }
        st.reported = true;
        if frames == st.frames {
            self.status = "no audio is arriving from the input".into();
            return;
        }
        let loopback = self
            .audio
            .input_device()
            .is_some_and(|i| self.audio.inputs[i].loopback);
        self.status = "the input is exactly silent".into();
        if cfg!(target_os = "macos") {
            let permission = match loopback {
                true => "Screen & System Audio Recording",
                false => "Microphone",
            };
            self.status += &format!(
                ". If sound should be there, macOS is withholding it: grant your terminal \
                 access in System Settings > Privacy & Security > {permission}"
            );
        }
    }

    fn feedback_matrix(&mut self, ui: &mut egui::Ui) {
        ui.label("feedback (row = source, column = destination)");
        egui::Grid::new("fb").show(ui, |ui| {
            ui.label("");
            for dst in 0..VOICES {
                ui.colored_label(COLORS[dst], format!("{}", dst + 1));
            }
            ui.end_row();
            for src in 0..VOICES {
                ui.colored_label(COLORS[src], format!("{}", src + 1));
                for dst in 0..VOICES {
                    let cell = &mut self.feedback[src][dst];
                    if ui
                        .add(egui::DragValue::new(cell).range(0.0..=1.0).speed(0.01))
                        .changed()
                    {
                        let amount = *cell;
                        self.send(EngineCmd::Feedback { src, dst, amount });
                    }
                }
                ui.end_row();
            }
        });
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // rec_once clears rec on the audio thread; follow it.
        for i in 0..VOICES {
            self.voices[i].rec = self.audio.handle.rec(i);
        }
        self.collect_returned();
        self.check_input_arriving(ui.input(|i| i.time));
        let dropped = ui
            .ctx()
            .input(|i| i.raw.dropped_files.first().map(|f| f.path().to_path_buf()));
        if let Some(path) = dropped {
            self.load_wav(&path);
        }
        // Before the snapshot, so OSC changes go out as sent, per voice, and
        // are not mirrored across linked pairs.
        self.drain_osc();
        let before = self.voices;
        let fx_before = (self.voice_fx, self.master_fx);
        let now = ui.input(|i| i.time);
        if self.auto_random && now - self.last_random >= self.auto_seconds as f64 {
            self.randomize(None, now);
        }

        egui::Panel::left("controls")
            .resizable(false)
            .default_size(360.0)
            .show(ui, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.horizontal(|ui| {
                        for i in 0..VOICES {
                            let text =
                                egui::RichText::new(format!("voice {}", i + 1)).color(COLORS[i]);
                            ui.selectable_value(&mut self.selected, i, text);
                        }
                    });
                    ui.separator();
                    self.voice_controls(ui);
                    ui.separator();
                    self.feedback_matrix(ui);
                });
            });

        egui::CentralPanel::default().show(ui, |ui| {
            self.input_row(ui);
            self.random_row(ui);
            self.osc_row(ui);
            ui.horizontal_wrapped(|ui| {
                if ui.button("clear buffers").clicked() {
                    for b in 0..BUFFERS {
                        self.send(EngineCmd::ClearBuffer(b));
                    }
                    self.sample_seconds = None;
                    self.set_view_len(self.audio.buffer_seconds);
                }
                if ui.button("load wav...").clicked()
                    && let Some(path) = rfd::FileDialog::new()
                        .add_filter("WAV", &["wav", "wave"])
                        .pick_file()
                {
                    self.load_wav(&path);
                }
                if ui
                    .button("save loop...")
                    .on_hover_text(
                        "save the selected voice's loop region: stereo for a linked pair, \
                         else mono from the voice's buffer",
                    )
                    .clicked()
                    && let Some(path) = rfd::FileDialog::new()
                        .add_filter("WAV", &["wav"])
                        .set_file_name("softcut-loop.wav")
                        .save_file()
                {
                    let v = &self.voices[self.selected];
                    let linked = self.linked[self.selected / 2];
                    let (start, end, buffers) = loop_save_plan(v, linked, self.audio.sample_rate);
                    self.start_save(path, start, end, &buffers);
                }
                self.record_button(ui);
                if let Some(len) = self.sample_seconds
                    && ui.button("fit sample").clicked()
                {
                    self.set_view_len(len);
                }
                if ui.button("full buffer").clicked() {
                    self.set_view_len(self.audio.buffer_seconds);
                }
            });
            ui.label(&self.status);
            ui.add_space(6.0);
            self.waveform(ui);
            egui::CollapsingHeader::new("master effects")
                .id_salt("master fx")
                .show(ui, |ui| {
                    ui.label("stereo, on the mix of all voices; recorded output includes it");
                    fx_controls(ui, &mut self.master_fx, "master fx");
                });
            ui.add_space(6.0);
            ui.label(format!(
                "{} @ {} Hz, buffers 2 x {:.1} s. Waveform: click to cut voice {}, drag to set its \
                 loop. Thick playhead = recording. Drop a WAV on the window to load it.",
                self.audio.output_name(),
                self.audio.sample_rate,
                self.audio.buffer_seconds,
                self.selected + 1
            ));
        });

        let s = self.selected;
        if self.linked[s / 2] && self.voices[s] != before[s] {
            self.voices[s ^ 1] = self.voices[s].partner();
        }
        if self.linked[s / 2] && self.voice_fx[s] != fx_before.0[s] {
            self.voice_fx[s ^ 1] = self.voice_fx[s];
        }
        for v in 0..VOICES {
            for cmd in self.voice_fx[v].diff(&fx_before.0[v]) {
                self.audio.send_fx(FxTarget::Voice(v), cmd);
            }
        }
        for cmd in self.master_fx.diff(&fx_before.1) {
            self.audio.send_fx(FxTarget::Master, cmd);
        }
        let mut cmds = Vec::new();
        for i in 0..VOICES {
            self.voices[i].diff(i, Some(&before[i]), &mut cmds);
        }
        for c in cmds {
            self.send(c);
        }
        ui.ctx().request_repaint();
    }
}

/// Open the audio devices with the voices at their presets, or exit.
fn start_audio() -> Audio {
    let voices: [VoiceUi; VOICES] = std::array::from_fn(VoiceUi::preset);
    audio::start(|e: &mut Engine| {
        let mut cmds = Vec::new();
        for (i, v) in voices.iter().enumerate() {
            v.diff(i, None, &mut cmds);
        }
        cmds.into_iter().for_each(|c| e.apply(c));
    })
    .unwrap_or_else(|e| {
        eprintln!("audio: {e}");
        std::process::exit(1);
    })
}

impl App {
    /// The UI over `audio`, whose voices are at their presets.
    fn new(audio: Audio) -> Self {
        let status = match &audio.input_error {
            None => "Load a WAV, or turn the input on and press rec on voice 1.".into(),
            Some(e) => format!("Input unavailable: {e}. Load a WAV to play."),
        };
        let view_len = audio.buffer_seconds;
        Self {
            audio,
            voices: std::array::from_fn(VoiceUi::preset),
            linked: [true; VOICES / 2],
            feedback: [[0.0; VOICES]; VOICES],
            selected: 0,
            source: Source::Off,
            drag_from: None,
            status,
            meter: 0.0,
            sample_seconds: None,
            view_len,
            saving: None,
            stall: None,
            rng: Rng::from_time(),
            targets: Targets {
                rate: true,
                loop_region: true,
                pan_level: true,
                filter: true,
            },
            auto_random: false,
            auto_seconds: 4.0,
            last_random: 0.0,
            osc: None,
            osc_port: 9999,
            voice_fx: [FxUi::default(); VOICES],
            master_fx: FxUi::default(),
            phase_poll: PhasePoll::new(VOICES),
        }
    }
}

fn main() -> eframe::Result {
    let app = App::new(start_audio());
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1400.0, 760.0]),
        ..Default::default()
    };
    eframe::run_native(
        "softcut",
        options,
        Box::new(|cc| {
            cc.egui_ctx.set_theme(egui::Theme::Dark);
            Ok(Box::new(app))
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preset_engine(sr: f32) -> Engine {
        let mut e = audio::engine(sr);
        let mut cmds = Vec::new();
        for i in 0..VOICES {
            VoiceUi::preset(i).diff(i, None, &mut cmds);
        }
        cmds.into_iter().for_each(|c| e.apply(c));
        e
    }

    /// The presets play what a load puts in the buffers, and record nothing.
    #[test]
    fn presets_play_loaded_content() {
        let sr = 48000.0;
        let mut e = preset_engine(sr);
        for b in 0..BUFFERS {
            for (i, x) in e.buffer_mut(b).iter_mut().enumerate() {
                *x = 0.5 * (i as f32 * 0.03).sin();
            }
        }
        let mut out = [0.0; 1024];
        let mut peak = 0.0f32;
        for _ in 0..(sr as usize / 512) {
            e.process(&[0.0; 1024], &mut out);
            peak = out.iter().fold(peak, |m, x| m.max(x.abs()));
        }
        assert!(peak > 0.05, "output peak {peak}");
        assert!(
            e.voices().iter().all(|v| !v.rec()),
            "a preset records at startup"
        );
    }

    #[test]
    fn presets_are_stereo_pairs() {
        for pair in [0, 2] {
            let (a, b) = (VoiceUi::preset(pair), VoiceUi::preset(pair + 1));
            assert_eq!((a.buffer, b.buffer), (0, 1));
            assert_eq!((a.pan, b.pan), (-1.0, 1.0));
            assert_eq!(b, a.partner());
            assert_eq!(b.partner(), a);
        }
    }

    /// Recording on voices 1+2 puts the left input in buffer L and the right in R.
    #[test]
    fn linked_pair_records_stereo_input() {
        let mut e = preset_engine(48000.0);
        for i in 0..2 {
            let old = VoiceUi::preset(i);
            let new = VoiceUi { rec: true, ..old };
            let mut cmds = Vec::new();
            new.diff(i, Some(&old), &mut cmds);
            cmds.into_iter().for_each(|c| e.apply(c));
        }
        let input: Vec<f32> = (0..9600).flat_map(|_| [0.1, -0.2]).collect();
        let mut out = vec![0.0; 9600 * 2];
        e.process(&input, &mut out);
        // Fixed quirks keep polarity; soft clip gain is 1.2 below the knee.
        let (l, r) = (e.buffers()[0][8000], e.buffers()[1][8000]);
        assert!(
            (l - 0.12).abs() < 1e-2 && (r + 0.24).abs() < 1e-2,
            "L {l}, R {r}"
        );
    }

    const ALL: Targets = Targets {
        rate: true,
        loop_region: true,
        pan_level: true,
        filter: true,
    };

    #[test]
    fn randomized_values_stay_in_range() {
        let mut rng = Rng(7);
        for _ in 0..1000 {
            let mut v = VoiceUi::preset(0);
            v.randomize(&mut rng, ALL, 3.0);
            assert!(RATES.contains(&v.rate.abs()), "rate {}", v.rate);
            assert!(v.loop_start >= 0.0 && v.loop_end <= 3.0 + 1e-6, "{v:?}");
            assert!(v.loop_end - v.loop_start >= MIN_LOOP - 1e-6, "{v:?}");
            assert!((-1.0..=1.0).contains(&v.pan) && (0.4..=0.9).contains(&v.level));
            assert!((200.0..=12000.0).contains(&v.post_fc), "fc {}", v.post_fc);
            let mixes = v.post_lp + v.post_bp + v.post_hp + v.post_dry;
            assert_eq!(mixes, 1.0, "one filter mode at a time");
        }
    }

    #[test]
    fn untargeted_settings_are_untouched() {
        let mut rng = Rng(3);
        let base = VoiceUi::preset(2);
        let mut v = base;
        let t = Targets {
            rate: true,
            loop_region: false,
            pan_level: false,
            filter: false,
        };
        v.randomize(&mut rng, t, 3.0);
        assert_eq!(
            VoiceUi {
                rate: base.rate,
                ..v
            },
            base
        );
    }

    #[test]
    fn short_content_still_gives_a_valid_loop() {
        let mut rng = Rng(11);
        let mut v = VoiceUi::preset(0);
        v.randomize(&mut rng, ALL, 0.02);
        assert_eq!((v.loop_start, v.loop_end), (0.0, 0.02));
    }

    #[test]
    fn linked_pairs_stay_mirrored() {
        let mut voices: [VoiceUi; VOICES] = std::array::from_fn(VoiceUi::preset);
        let mut rng = Rng(5);
        randomize_voices(&mut voices, &[true, false], None, &mut rng, ALL, 3.0);
        assert_eq!(voices[1], voices[0].partner());
        // The unlinked pair is drawn independently.
        assert_ne!(voices[3], voices[2].partner());
        randomize_voices(&mut voices, &[true, true], Some(3), &mut rng, ALL, 3.0);
        assert_eq!(voices[2], voices[3].partner());
    }

    #[test]
    fn fade_zones_follow_direction_and_loop_flag() {
        let v = VoiceUi {
            loop_start: 1.0,
            loop_end: 3.0,
            fade_time: 0.2,
            rate: 1.5,
            ..VoiceUi::preset(0)
        };
        let ends = |z: &FadeZone| {
            let pts: Vec<_> = z.points(v.fade_time).map(|(t, f)| (t, gain(f))).collect();
            (pts[0], *pts.last().unwrap())
        };
        let zones = v.fade_zones();
        // Forward: fade out past the end, fade in from the start.
        assert_eq!((zones[0].origin, zones[0].fading_in), (3.0, false));
        assert_eq!(ends(&zones[0]), ((3.0, 1.0), (3.2, 0.0)));
        assert_eq!((zones[1].origin, zones[1].fading_in), (1.0, true));
        assert_eq!(ends(&zones[1]), ((1.0, 0.0), (1.2, 1.0)));
        // Reverse: mirrored, past the start and in from the end.
        let rev = VoiceUi { rate: -0.5, ..v }.fade_zones();
        assert_eq!((rev[0].origin, rev[0].dir), (1.0, -1.0));
        assert_eq!(ends(&rev[1]), ((3.0, 0.0), (2.8, 1.0)));
        // One-shot: only the fade-out; no fade, no zones.
        assert_eq!(
            VoiceUi {
                loop_on: false,
                ..v
            }
            .fade_zones()
            .len(),
            1
        );
        assert!(
            VoiceUi {
                fade_time: 0.0,
                ..v
            }
            .fade_zones()
            .is_empty()
        );
    }

    #[test]
    fn loop_save_plan_takes_the_region_and_channels() {
        let v = VoiceUi {
            loop_start: 1.0,
            loop_end: 1.5,
            buffer: 1,
            ..VoiceUi::preset(0)
        };
        assert_eq!(loop_save_plan(&v, true, 1000.0), (1000, 1500, vec![0, 1]));
        assert_eq!(loop_save_plan(&v, false, 1000.0), (1000, 1500, vec![1]));
        let reversed = VoiceUi {
            loop_start: 1.5,
            loop_end: 1.0,
            ..v
        };
        assert_eq!(loop_save_plan(&reversed, false, 1000.0).0, 1000);
    }

    #[test]
    fn fx_diff_sends_only_changes() {
        let old = FxUi::default();
        assert!(old.diff(&old).is_empty());
        let mut new = old;
        new.on[Fx::Delay as usize] = true;
        new.delay_time = 0.5;
        new.reverb_mix = 0.1;
        assert_eq!(
            new.diff(&old),
            vec![
                FxCmd::Enable(Fx::Delay, true),
                FxCmd::DelayTime(0.5),
                FxCmd::ReverbMix(0.1)
            ]
        );
    }
}
