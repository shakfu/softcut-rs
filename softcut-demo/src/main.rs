//! softcut demo: four voices over one shared buffer, with the buffer's
//! waveform, each voice's loop region and playhead drawn live.
//!
//! Waveform: click to cut the selected voice, drag to set its loop.
//! WAV files load into the buffer from the button or by dropping them on the window.

// Per-voice state lives in parallel arrays indexed by voice number.
#![allow(clippy::needless_range_loop)]

mod audio;
mod wav;

use std::path::Path;

use audio::{Audio, BUFFER_FRAMES, Source, VOICES, WAVE_BINS};
use eframe::egui::{self, Color32, Pos2, Rect, Sense, Stroke, Vec2};
use softcut::{Engine, EngineCmd, VoiceCmd};

const COLORS: [Color32; VOICES] = [
    Color32::from_rgb(230, 90, 80),
    Color32::from_rgb(80, 170, 230),
    Color32::from_rgb(120, 200, 110),
    Color32::from_rgb(220, 180, 60),
];

/// The UI's copy of each voice's settings. softcut has no getters for these;
/// the UI is the source of truth and pushes every change as a command.
#[derive(Clone, Copy, PartialEq)]
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
}

impl VoiceUi {
    fn preset(i: usize) -> Self {
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
            pan: 0.0,
            input_gain: 0.0,
            post_fc: 8000.0,
            post_rq: 2.0,
            post_lp: 0.0,
            post_hp: 0.0,
            post_bp: 0.0,
            post_dry: 1.0,
        };
        match i {
            0 => Self {
                input_gain: 1.0,
                ..base
            },
            1 => Self {
                rate: 0.5,
                pan: -0.6,
                level: 0.6,
                ..base
            },
            2 => Self {
                rate: -1.0,
                loop_start: 1.0,
                loop_end: 2.5,
                pan: 0.6,
                level: 0.5,
                ..base
            },
            _ => Self {
                rate: 2.0,
                play: false,
                level: 0.4,
                post_lp: 1.0,
                post_dry: 0.0,
                post_fc: 1500.0,
                ..base
            },
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
        push!(level, EngineCmd::Level(i, self.level));
        push!(pan, EngineCmd::Pan(i, self.pan));
        push!(input_gain, EngineCmd::InputGain(i, self.input_gain));
        push!(play, v(Play(self.play)));
        push!(rec, v(Rec(self.rec)));
    }
}

struct App {
    audio: Audio,
    voices: [VoiceUi; VOICES],
    feedback: [[f32; VOICES]; VOICES],
    selected: usize,
    source: Source,
    drag_from: Option<f32>,
    /// Result of the last WAV load.
    status: String,
    /// Displayed input level, linear, with meter ballistics applied.
    meter: f32,
    /// Length of the loaded sample, if one is loaded.
    sample_seconds: Option<f32>,
    /// Seconds from the buffer start shown on the waveform.
    view_len: f32,
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

    /// Load a WAV into the buffer, stop recording, and loop every voice over it.
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
        if let Err(e) = self.audio.handle.load(0, loaded.data) {
            self.status = format!("{name}: {e}");
            return;
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

    fn send(&mut self, cmd: EngineCmd) {
        // A full ring means the audio thread has stalled; the handle counts the
        // drop, and blocking the UI would not help.
        let _ = self.audio.handle.send(cmd);
    }

    fn waveform(&mut self, ui: &mut egui::Ui) {
        let size = Vec2::new(ui.available_width(), 220.0);
        let (resp, painter) = ui.allocate_painter(size, Sense::click_and_drag());
        let rect = resp.rect;
        let len = self.view_len;
        let x_of = |t: f32| rect.left() + t / len * rect.width();
        let t_of = |x: f32| ((x - rect.left()) / rect.width() * len).clamp(0.0, len);

        painter.rect_filled(rect, 4.0, ui.visuals().extreme_bg_color);
        for (i, v) in self.voices.iter().enumerate() {
            let band = Rect::from_x_y_ranges(x_of(v.loop_start)..=x_of(v.loop_end), rect.y_range());
            let alpha = if i == self.selected { 60 } else { 22 };
            painter.rect_filled(band, 0.0, COLORS[i].gamma_multiply_u8(alpha));
        }
        let mid = rect.center().y;
        let bin_w = rect.width() / WAVE_BINS as f32;
        let wave = ui.visuals().text_color();
        for b in 0..WAVE_BINS {
            let h = self.audio.meters.peak(b).min(1.0) * rect.height() * 0.5;
            let x = rect.left() + (b as f32 + 0.5) * bin_w;
            painter.line_segment(
                [Pos2::new(x, mid - h), Pos2::new(x, mid + h)],
                Stroke::new(bin_w.max(1.0), wave),
            );
        }
        for i in 0..VOICES {
            if !self.voices[i].play && !self.voices[i].rec {
                continue;
            }
            let t = self.audio.handle.position(i);
            if !(0.0..=len).contains(&t) {
                continue;
            }
            let width = if self.voices[i].rec { 3.0 } else { 1.5 };
            painter.vline(x_of(t), rect.y_range(), Stroke::new(width, COLORS[i]));
        }

        let label = egui::FontId::monospace(11.0);
        let dim = ui.visuals().weak_text_color();
        let pad = Vec2::new(4.0, -2.0);
        painter.text(
            rect.left_bottom() + pad,
            egui::Align2::LEFT_BOTTOM,
            "0 s",
            label.clone(),
            dim,
        );
        painter.text(
            rect.right_bottom() + Vec2::new(-pad.x, pad.y),
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
                self.send(EngineCmd::Voice(sel, VoiceCmd::CutTo(t)));
            }
        }
        if resp.drag_stopped() {
            self.drag_from = None;
        }
    }

    fn voice_controls(&mut self, ui: &mut egui::Ui) {
        let secs = self.content_seconds();
        let i = self.selected;
        let v = &mut self.voices[i];
        ui.horizontal(|ui| {
            ui.toggle_value(&mut v.play, "play");
            ui.toggle_value(&mut v.rec, "rec");
            ui.checkbox(&mut v.loop_on, "loop");
        });
        let mut cut = false;
        let mut rec_once = false;
        ui.horizontal(|ui| {
            cut = ui.button("cut to start").clicked();
            rec_once = ui
                .button("rec once")
                .on_hover_text("record one pass of the loop")
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
        let start = v.loop_start;
        if cut {
            self.send(EngineCmd::Voice(i, VoiceCmd::CutTo(start)));
        }
        if rec_once {
            self.send(EngineCmd::Voice(i, VoiceCmd::RecOnce(true)));
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
        // Replaced buffers come back here to be freed off the audio thread.
        while self.audio.handle.returned().is_some() {}
        let dropped = ui
            .ctx()
            .input(|i| i.raw.dropped_files.first().map(|f| f.path().to_path_buf()));
        if let Some(path) = dropped {
            self.load_wav(&path);
        }
        let before = self.voices;

        egui::Panel::left("controls")
            .resizable(false)
            .default_size(330.0)
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
            ui.horizontal(|ui| {
                ui.label("input");
                let old = self.source;
                ui.add_enabled_ui(self.audio.input_error.is_none(), |ui| {
                    ui.selectable_value(&mut self.source, Source::Mic, "mic")
                        .on_disabled_hover_text(self.audio.input_error.clone().unwrap_or_default());
                });
                ui.selectable_value(&mut self.source, Source::Off, "off");
                if self.source != old
                    && let Err(e) = self.audio.set_source(self.source)
                {
                    self.status = format!("mic: {e}");
                    self.source = old;
                }
                self.input_meter(ui);
                if ui.button("clear buffer").clicked() {
                    self.send(EngineCmd::ClearBuffer(0));
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
                if let Some(len) = self.sample_seconds
                    && ui.button("fit sample").clicked()
                {
                    self.set_view_len(len);
                }
                if ui.button("full buffer").clicked() {
                    self.set_view_len(self.audio.buffer_seconds);
                }
                ui.label(&self.status);
            });
            ui.add_space(6.0);
            self.waveform(ui);
            ui.add_space(6.0);
            ui.label(format!(
                "{} @ {} Hz, buffer {:.1} s. Waveform: click to cut voice {}, drag to set its loop. \
                 Thick playhead = recording. Drop a WAV on the window to load it.",
                self.audio.output_name,
                self.audio.sample_rate,
                self.audio.buffer_seconds,
                self.selected + 1
            ));
        });

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
        feedback: [[0.0; VOICES]; VOICES],
        selected: 0,
        source: Source::Off,
        drag_from: None,
        status: match &input_error {
            None => "Load a WAV, or choose mic and press rec on voice 1.".into(),
            Some(e) => format!("Mic unavailable: {e}. Load a WAV to play."),
        },
        meter: 0.0,
        sample_seconds: None,
        view_len: full,
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1100.0, 620.0]),
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
    use softcut::EngineConfig;

    /// The presets play what a load puts in the buffer, and record nothing.
    #[test]
    fn presets_play_loaded_content() {
        let sr = 48000.0;
        let mut e = Engine::new(EngineConfig {
            sample_rate: sr,
            voices: VOICES,
            buffers: 1,
            buffer_frames: BUFFER_FRAMES,
            block_size: 64,
            out_channels: 2,
        });
        for (i, x) in e.buffer_mut(0).iter_mut().enumerate() {
            *x = 0.5 * (i as f32 * 0.03).sin();
        }
        let mut cmds = Vec::new();
        for i in 0..VOICES {
            VoiceUi::preset(i).diff(i, None, &mut cmds);
        }
        cmds.into_iter().for_each(|c| e.apply(c));

        let mut out = [0.0; 1024];
        let mut peak = 0.0f32;
        for _ in 0..(sr as usize / 512) {
            e.process(&[0.0; 512], &mut out);
            peak = out.iter().fold(peak, |m, x| m.max(x.abs()));
        }
        assert!(peak > 0.05, "output peak {peak}");
        assert!(
            e.voices().iter().all(|v| !v.rec()),
            "a preset records at startup"
        );
    }
}
