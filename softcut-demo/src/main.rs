//! softcut demo: four voices over a stereo pair of buffers (L and R), with
//! both buffers' waveforms, each voice's loop region and playhead drawn live.
//! Voices 1+2 and 3+4 are linked stereo pairs by default.
//!
//! Waveform: click to cut the selected voice, drag to set its loop.
//! WAV files load into the buffers from the button or by dropping them on the
//! window, and save from the buffers with "save wav...".

// Per-voice state lives in parallel arrays indexed by voice number.
#![allow(clippy::needless_range_loop)]

mod audio;
mod resample;
mod wav;

use std::path::{Path, PathBuf};

use audio::{Audio, BUFFER_FRAMES, BUFFERS, InputDevice, Source, VOICES, WAVE_BINS};
use eframe::egui::{self, Color32, Pos2, Rect, Sense, Stroke, Vec2};
use softcut::rt::Returned;
use softcut::{Engine, EngineCmd, FadeShape, VoiceCmd};

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

/// A save waiting for both buffers' snapshots to come back.
struct PendingSave {
    path: PathBuf,
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
    fn start_save(&mut self, path: PathBuf) {
        if self.saving.is_some() {
            self.status = "a save is already in progress".into();
            return;
        }
        let frames =
            ((self.content_seconds() * self.audio.sample_rate) as usize).min(BUFFER_FRAMES);
        for b in 0..BUFFERS {
            if let Err(e) = self.audio.handle.snapshot(b, vec![0.0; frames].into()) {
                self.status = format!("save: {e}");
                return;
            }
        }
        self.saving = Some(PendingSave {
            path,
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
        if self
            .saving
            .as_ref()
            .is_some_and(|s| s.parts.iter().all(Option::is_some))
        {
            let save = self.saving.take().unwrap();
            let [l, r] = save.parts.map(Option::unwrap);
            let name = save
                .path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            let sr = self.audio.sample_rate;
            self.status = match wav::save(&save.path, &l, &r, sr as u32) {
                Ok(()) => format!("saved {name}: {:.1} s", l.len() as f32 / sr),
                Err(e) => format!("save {name}: {e}"),
            };
        }
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
                let t = self.audio.handle.position(i);
                if !(0.0..=len).contains(&t) {
                    continue;
                }
                let width = if v.rec { 3.0 } else { 1.5 };
                painter.vline(x_of(t), lane.y_range(), Stroke::new(width, COLORS[i]));
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
                egui::Slider::new(&mut v.fade_time, 0.0..=1.0).suffix(" s"),
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

    /// Input device, channel pair, on/off and level.
    fn input_row(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
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
        let before = self.voices;

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
            ui.horizontal(|ui| {
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
                    .button("save wav...")
                    .on_hover_text("save both buffers, over the loaded sample's length, as stereo")
                    .clicked()
                    && let Some(path) = rfd::FileDialog::new()
                        .add_filter("WAV", &["wav"])
                        .set_file_name("softcut.wav")
                        .save_file()
                {
                    self.start_save(path);
                }
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

fn main() -> eframe::Result {
    let voices: [VoiceUi; VOICES] = std::array::from_fn(VoiceUi::preset);
    let audio = audio::start(|e: &mut Engine| {
        let mut cmds = Vec::new();
        for (i, v) in voices.iter().enumerate() {
            v.diff(i, None, &mut cmds);
        }
        cmds.into_iter().for_each(|c| e.apply(c));
    })
    .unwrap_or_else(|e| {
        eprintln!("audio: {e}");
        std::process::exit(1);
    });

    let input_error = audio.input_error.clone();
    let full = audio.buffer_seconds;
    let app = App {
        audio,
        voices,
        linked: [true; VOICES / 2],
        feedback: [[0.0; VOICES]; VOICES],
        selected: 0,
        source: Source::Off,
        drag_from: None,
        status: match &input_error {
            None => "Load a WAV, or turn the input on and press rec on voice 1.".into(),
            Some(e) => format!("Input unavailable: {e}. Load a WAV to play."),
        },
        meter: 0.0,
        sample_seconds: None,
        view_len: full,
        saving: None,
        stall: None,
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1150.0, 680.0]),
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
}
