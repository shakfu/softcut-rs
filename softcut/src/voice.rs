//! One softcut voice. Port of `Voice.cpp`.

use crate::dsp::LogRamp;
use crate::head::ReadWriteHead;
use crate::svf::Svf;

/// A single voice setting or action, for hosts that queue changes to the audio
/// thread. `Copy` and allocation-free; applied with [`Voice::apply`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum VoiceCmd {
    Rate(f32),
    LoopStart(f32),
    LoopEnd(f32),
    Loop(bool),
    FadeTime(f32),
    RecLevel(f32),
    PreLevel(f32),
    Rec(bool),
    RecOnce(bool),
    Play(bool),
    RecOffset(f32),
    RecPreSlewTime(f32),
    RateSlewTime(f32),
    PhaseQuant(f32),
    PhaseOffset(f32),
    PreFilterFc(f32),
    PreFilterRq(f32),
    PreFilterLp(f32),
    PreFilterHp(f32),
    PreFilterBp(f32),
    PreFilterBr(f32),
    PreFilterDry(f32),
    PreFilterFcMod(f32),
    PostFilterFc(f32),
    PostFilterRq(f32),
    PostFilterLp(f32),
    PostFilterHp(f32),
    PostFilterBp(f32),
    PostFilterBr(f32),
    PostFilterDry(f32),
    CutTo(f32),
    Stop,
    Reset,
}

/// Which upstream softcut-lib behaviours a voice reproduces.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Quirks {
    /// Match softcut-lib sample for sample: recorded material is
    /// polarity-inverted, and `reset` leaves a 0.1 s fade time.
    #[default]
    Upstream,
    /// Record with the input's polarity, and leave the 0.01 s fade time that
    /// `reset` sets.
    Fixed,
}

/// A crossfading, resampling read/write head over a caller-owned buffer, with
/// pre (input) and post (output) state-variable filters.
///
/// The voice holds no buffer. [`process_block`](Self::process_block) borrows
/// one per call, so several voices can share a buffer by being processed in
/// turn. Times and positions are in seconds.
#[derive(Clone)]
pub struct Voice {
    sample_rate: f32,
    head: ReadWriteHead,
    svf_pre: Svf,
    svf_post: Svf,
    rate_ramp: LogRamp,
    pre_ramp: LogRamp,
    rec_ramp: LogRamp,
    svf_pre_fc_base: f32,
    svf_pre_fc_mod: f32,
    svf_pre_dry: f32,
    svf_post_dry: f32,
    phase_quant: f64,
    phase_offset: f32,
    raw_phase: f64,
    quant_phase: f64,
    play: bool,
    rec: bool,
    quirks: Quirks,
}

impl Voice {
    /// A voice that reproduces upstream behaviour ([`Quirks::Upstream`]).
    pub fn new(sample_rate: f32) -> Self {
        Self::with_quirks(sample_rate, Quirks::Upstream)
    }

    pub fn with_quirks(sample_rate: f32, quirks: Quirks) -> Self {
        let mut v = Self {
            sample_rate: 48000.0,
            head: ReadWriteHead::new(quirks),
            svf_pre: Svf::new(),
            svf_post: Svf::new(),
            rate_ramp: LogRamp::new(48000.0, 0.1),
            pre_ramp: LogRamp::new(48000.0, 0.1),
            rec_ramp: LogRamp::new(48000.0, 0.1),
            svf_pre_fc_base: 16000.0,
            svf_pre_fc_mod: 1.0,
            svf_pre_dry: 0.0,
            svf_post_dry: 1.0,
            phase_quant: 0.0,
            phase_offset: 0.0,
            raw_phase: 0.0,
            quant_phase: 0.0,
            play: false,
            rec: false,
            quirks,
        };
        v.reset();
        v.set_sample_rate(sample_rate);
        v
    }

    /// Restore defaults and stop both subheads. Keeps the sample rate, loop
    /// flag and the pre-filter base cutoff, as upstream does.
    ///
    /// With [`Quirks::Upstream`] the fade time after reset is 0.1 s: upstream
    /// sets 0.01 s and then the head's own init overrides it.
    pub fn reset(&mut self) {
        self.svf_pre.clear_state();
        self.svf_pre.set_lp_mix(1.0);
        self.svf_pre.set_hp_mix(0.0);
        self.svf_pre.set_bp_mix(0.0);
        self.svf_pre.set_br_mix(0.0);
        self.svf_pre.set_rq(4.0);
        self.svf_pre.set_fc(self.svf_pre_fc_base);
        self.svf_pre_fc_mod = 1.0;
        self.svf_pre_dry = 0.0;

        self.svf_post.clear_state();
        self.svf_post.set_lp_mix(0.0);
        self.svf_post.set_hp_mix(0.0);
        self.svf_post.set_bp_mix(0.0);
        self.svf_post.set_br_mix(0.0);
        self.svf_post.set_rq(4.0);
        self.svf_post.set_fc(12000.0);
        self.svf_post_dry = 1.0;

        self.rate_ramp.reset(1.0);
        self.rec_ramp.reset(0.0);
        self.pre_ramp.reset(0.0);

        self.set_fade_time(0.01);
        self.set_rec_pre_slew_time(0.001);
        self.set_rate_slew_time(0.001);

        self.head.set_rec_offset_samples(-8);

        self.phase_quant = 0.0;
        self.phase_offset = 0.0;
        self.raw_phase = 0.0;
        self.quant_phase = 0.0;
        self.rec = false;
        self.play = false;

        self.head.init();
        if self.quirks == Quirks::Fixed {
            self.set_fade_time(0.01);
        }
    }

    /// Process one mono block. `buf` is the audio buffer the heads read and
    /// write; `output` receives `input.len()` samples.
    ///
    /// # Panics
    /// If `buf.len()` is not a positive power of two, or `output` is shorter
    /// than `input`.
    pub fn process_block(&mut self, buf: &mut [f32], input: &[f32], output: &mut [f32]) {
        assert!(
            buf.len().is_power_of_two(),
            "softcut buffer length must be a power of two, got {}",
            buf.len()
        );
        let output = &mut output[..input.len()];
        let mask = buf.len() - 1;
        let (play, rec) = (self.play, self.rec);

        for (&x_in, out) in input.iter().zip(output.iter_mut()) {
            let x = self.svf_pre.next(x_in) + x_in * self.svf_pre_dry;
            self.head.set_rate(self.rate_ramp.update() as f64);
            self.head.set_pre(self.pre_ramp.update());
            self.head.set_rec(self.rec_ramp.update());
            let y = match (play, rec) {
                (true, true) => self.head.process_sample(x, buf, mask),
                (true, false) => self.head.process_sample_no_write(buf, mask),
                (false, true) => self.head.process_sample_no_read(x, buf, mask),
                (false, false) => 0.0,
            };
            *out = self.svf_post.next(y) + y * self.svf_post_dry;
        }

        // Upstream refreshes the quantized phase every sample; only the last
        // value is observable, so it is computed once here.
        self.raw_phase = self.head.active_phase();
        self.update_quant_phase();

        if self.rec && self.head.rec_once_done() {
            self.rec = false;
            self.head.set_rec_once_flag(false);
        }
    }

    pub fn apply(&mut self, cmd: VoiceCmd) {
        use VoiceCmd::*;
        match cmd {
            Rate(x) => self.set_rate(x),
            LoopStart(x) => self.set_loop_start(x),
            LoopEnd(x) => self.set_loop_end(x),
            Loop(b) => self.set_loop(b),
            FadeTime(x) => self.set_fade_time(x),
            RecLevel(x) => self.set_rec_level(x),
            PreLevel(x) => self.set_pre_level(x),
            Rec(b) => self.set_rec(b),
            RecOnce(b) => self.set_rec_once(b),
            Play(b) => self.set_play(b),
            RecOffset(x) => self.set_rec_offset(x),
            RecPreSlewTime(x) => self.set_rec_pre_slew_time(x),
            RateSlewTime(x) => self.set_rate_slew_time(x),
            PhaseQuant(x) => self.set_phase_quant(x),
            PhaseOffset(x) => self.set_phase_offset(x),
            PreFilterFc(x) => self.set_pre_filter_fc(x),
            PreFilterRq(x) => self.set_pre_filter_rq(x),
            PreFilterLp(x) => self.set_pre_filter_lp(x),
            PreFilterHp(x) => self.set_pre_filter_hp(x),
            PreFilterBp(x) => self.set_pre_filter_bp(x),
            PreFilterBr(x) => self.set_pre_filter_br(x),
            PreFilterDry(x) => self.set_pre_filter_dry(x),
            PreFilterFcMod(x) => self.set_pre_filter_fc_mod(x),
            PostFilterFc(x) => self.set_post_filter_fc(x),
            PostFilterRq(x) => self.set_post_filter_rq(x),
            PostFilterLp(x) => self.set_post_filter_lp(x),
            PostFilterHp(x) => self.set_post_filter_hp(x),
            PostFilterBp(x) => self.set_post_filter_bp(x),
            PostFilterBr(x) => self.set_post_filter_br(x),
            PostFilterDry(x) => self.set_post_filter_dry(x),
            CutTo(x) => self.cut_to(x),
            Stop => self.stop(),
            Reset => self.reset(),
        }
    }

    pub fn set_sample_rate(&mut self, hz: f32) {
        self.sample_rate = hz;
        self.rate_ramp.set_sample_rate(hz);
        self.pre_ramp.set_sample_rate(hz);
        self.rec_ramp.set_sample_rate(hz);
        self.head.set_sample_rate(hz);
        self.svf_pre.set_sample_rate(hz);
        self.svf_post.set_sample_rate(hz);
    }

    /// Playback rate; negative plays backwards. Slewed by `rate_slew_time`.
    pub fn set_rate(&mut self, rate: f32) {
        self.rate_ramp.set_target(rate);
        self.update_pre_svf_fc();
    }

    pub fn set_loop_start(&mut self, sec: f32) {
        self.head.set_loop_start_seconds(sec);
    }

    pub fn set_loop_end(&mut self, sec: f32) {
        self.head.set_loop_end_seconds(sec);
    }

    pub fn set_loop(&mut self, on: bool) {
        self.head.set_loop_flag(on);
    }

    /// Crossfade time for cuts and loop wraps.
    pub fn set_fade_time(&mut self, sec: f32) {
        self.head.set_fade_time(sec);
    }

    /// Gain of new input written to the buffer.
    pub fn set_rec_level(&mut self, amp: f32) {
        self.rec_ramp.set_target(amp);
    }

    /// Gain applied to existing buffer content while recording (overdub).
    pub fn set_pre_level(&mut self, amp: f32) {
        self.pre_ramp.set_target(amp);
    }

    pub fn set_rec(&mut self, on: bool) {
        if self.rec {
            if !(on || self.play) {
                self.head.stop();
            }
        } else if on && !self.play {
            self.head.run();
        }
        self.rec = on;
        if !on && self.head.rec_once_active() {
            self.head.set_rec_once_flag(false);
        }
    }

    /// Record one pass of the loop, starting at the next cut or wrap.
    pub fn set_rec_once(&mut self, on: bool) {
        self.head.set_rec_once_flag(on);
        if on {
            self.set_rec(true);
        }
    }

    pub fn set_play(&mut self, on: bool) {
        if self.play {
            if !(on || self.rec) {
                self.head.stop();
            }
        } else if on && !self.rec {
            self.head.run();
        }
        self.play = on;
    }

    /// Offset of the write head from the read head.
    pub fn set_rec_offset(&mut self, sec: f32) {
        self.head
            .set_rec_offset_samples((sec * self.sample_rate) as i32);
    }

    pub fn set_rec_pre_slew_time(&mut self, sec: f32) {
        self.rec_ramp.set_time(sec);
        self.pre_ramp.set_time(sec);
    }

    pub fn set_rate_slew_time(&mut self, sec: f32) {
        self.rate_ramp.set_time(sec);
    }

    /// Quantization step of [`quant_phase`](Self::quant_phase); 0 disables it.
    pub fn set_phase_quant(&mut self, sec: f32) {
        self.phase_quant = sec as f64;
    }

    pub fn set_phase_offset(&mut self, sec: f32) {
        self.phase_offset = sec * self.sample_rate;
    }

    /// Base cutoff of the input filter, before rate tracking.
    pub fn set_pre_filter_fc(&mut self, hz: f32) {
        self.svf_pre_fc_base = hz;
        self.update_pre_svf_fc();
    }
    pub fn set_pre_filter_rq(&mut self, x: f32) {
        self.svf_pre.set_rq(x);
    }
    pub fn set_pre_filter_lp(&mut self, x: f32) {
        self.svf_pre.set_lp_mix(x);
    }
    pub fn set_pre_filter_hp(&mut self, x: f32) {
        self.svf_pre.set_hp_mix(x);
    }
    pub fn set_pre_filter_bp(&mut self, x: f32) {
        self.svf_pre.set_bp_mix(x);
    }
    pub fn set_pre_filter_br(&mut self, x: f32) {
        self.svf_pre.set_br_mix(x);
    }
    pub fn set_pre_filter_dry(&mut self, x: f32) {
        self.svf_pre_dry = x;
    }
    /// How far the input cutoff follows `|rate|` below 1: 0 = not at all, 1 = fully.
    pub fn set_pre_filter_fc_mod(&mut self, x: f32) {
        self.svf_pre_fc_mod = x;
    }

    pub fn set_post_filter_fc(&mut self, hz: f32) {
        self.svf_post.set_fc(hz);
    }
    pub fn set_post_filter_rq(&mut self, x: f32) {
        self.svf_post.set_rq(x);
    }
    pub fn set_post_filter_lp(&mut self, x: f32) {
        self.svf_post.set_lp_mix(x);
    }
    pub fn set_post_filter_hp(&mut self, x: f32) {
        self.svf_post.set_hp_mix(x);
    }
    pub fn set_post_filter_bp(&mut self, x: f32) {
        self.svf_post.set_bp_mix(x);
    }
    pub fn set_post_filter_br(&mut self, x: f32) {
        self.svf_post.set_br_mix(x);
    }
    pub fn set_post_filter_dry(&mut self, x: f32) {
        self.svf_post_dry = x;
    }

    /// Jump to `sec` with a crossfade.
    pub fn cut_to(&mut self, sec: f32) {
        self.head.cut_to_pos(sec);
    }

    /// Stop both subheads immediately, without a fade.
    pub fn stop(&mut self) {
        self.head.stop();
    }

    // Getters read the DSP state, not a record of the last value set, so they
    // report what the voice actually does: e.g. `fade_time` is 0.1 after
    // `reset` (see there), and `post_filter_fc` is clamped.

    /// Rate target; the head reaches it over `rate_slew_time`.
    pub fn rate(&self) -> f32 {
        self.rate_ramp.target()
    }

    pub fn loop_start(&self) -> f32 {
        self.head.loop_start_seconds()
    }

    pub fn loop_end(&self) -> f32 {
        self.head.loop_end_seconds()
    }

    pub fn looping(&self) -> bool {
        self.head.loop_flag()
    }

    pub fn fade_time(&self) -> f32 {
        self.head.fade_time()
    }

    /// Rec level target; reached over `rec_pre_slew_time`.
    pub fn rec_level(&self) -> f32 {
        self.rec_ramp.target()
    }

    /// Pre level target; reached over `rec_pre_slew_time`.
    pub fn pre_level(&self) -> f32 {
        self.pre_ramp.target()
    }

    /// True from `set_rec_once(true)` until the pass completes.
    pub fn rec_once(&self) -> bool {
        self.head.rec_once_active()
    }

    /// Whole samples, converted back to seconds.
    pub fn rec_offset(&self) -> f32 {
        self.head.rec_offset_samples() as f32 / self.sample_rate
    }

    pub fn rec_pre_slew_time(&self) -> f32 {
        self.rec_ramp.time()
    }

    pub fn rate_slew_time(&self) -> f32 {
        self.rate_ramp.time()
    }

    pub fn phase_quant(&self) -> f32 {
        self.phase_quant as f32
    }

    pub fn phase_offset(&self) -> f32 {
        self.phase_offset / self.sample_rate
    }

    /// Base cutoff as set; the filter runs at this lowered by rate tracking.
    pub fn pre_filter_fc(&self) -> f32 {
        self.svf_pre_fc_base
    }
    pub fn pre_filter_rq(&self) -> f32 {
        self.svf_pre.rq()
    }
    pub fn pre_filter_lp(&self) -> f32 {
        self.svf_pre.lp_mix()
    }
    pub fn pre_filter_hp(&self) -> f32 {
        self.svf_pre.hp_mix()
    }
    pub fn pre_filter_bp(&self) -> f32 {
        self.svf_pre.bp_mix()
    }
    pub fn pre_filter_br(&self) -> f32 {
        self.svf_pre.br_mix()
    }
    pub fn pre_filter_dry(&self) -> f32 {
        self.svf_pre_dry
    }
    pub fn pre_filter_fc_mod(&self) -> f32 {
        self.svf_pre_fc_mod
    }

    /// Clamped to [10 Hz, 0.4 * sample rate].
    pub fn post_filter_fc(&self) -> f32 {
        self.svf_post.fc()
    }
    pub fn post_filter_rq(&self) -> f32 {
        self.svf_post.rq()
    }
    pub fn post_filter_lp(&self) -> f32 {
        self.svf_post.lp_mix()
    }
    pub fn post_filter_hp(&self) -> f32 {
        self.svf_post.hp_mix()
    }
    pub fn post_filter_bp(&self) -> f32 {
        self.svf_post.bp_mix()
    }
    pub fn post_filter_br(&self) -> f32 {
        self.svf_post.br_mix()
    }
    pub fn post_filter_dry(&self) -> f32 {
        self.svf_post_dry
    }

    pub fn play(&self) -> bool {
        self.play
    }

    pub fn rec(&self) -> bool {
        self.rec
    }

    /// Current position of the active subhead.
    pub fn position(&self) -> f32 {
        (self.head.active_phase() / self.sample_rate as f64) as f32
    }

    /// Position at the end of the last processed block.
    pub fn saved_position(&self) -> f32 {
        (self.raw_phase / self.sample_rate as f64) as f32
    }

    /// Position at the end of the last block, floored to `phase_quant` after
    /// adding `phase_offset`.
    pub fn quant_phase(&self) -> f64 {
        self.quant_phase
    }

    pub fn quirks(&self) -> Quirks {
        self.quirks
    }

    pub fn sample_rate(&self) -> f32 {
        self.sample_rate
    }

    // Uses the head's current (slewed) rate, not the target just set.
    fn update_pre_svf_fc(&mut self) {
        let base = self.svf_pre_fc_base;
        let tracked = base.min(base * (self.head.rate() as f32).abs());
        self.svf_pre
            .set_fc(base + self.svf_pre_fc_mod * (tracked - base));
    }

    fn update_quant_phase(&mut self) {
        let phase = self.head.active_phase();
        self.quant_phase = if self.phase_quant == 0.0 {
            phase / self.sample_rate as f64
        } else {
            let tmp =
                (phase + self.phase_offset as f64) / (self.sample_rate as f64 * self.phase_quant);
            tmp.floor() * self.phase_quant
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rec_once_clears_rec_after_one_pass() {
        let mut v = Voice::new(48000.0);
        let mut buf = vec![0.0; 1 << 14];
        v.set_loop_end(0.1);
        v.set_loop(true);
        v.set_rec_level(1.0);
        v.set_play(true);
        v.cut_to(0.0);
        v.set_rec_once(true);
        let (inp, mut out) = ([0.5; 512], [0.0; 512]);
        let mut blocks = 0;
        while v.rec() {
            v.process_block(&mut buf, &inp, &mut out);
            blocks += 1;
            assert!(blocks < 100, "rec_once never finished");
        }
        // One 0.1 s pass is ~10 blocks; it cannot finish before the loop wraps.
        assert!(blocks >= 9, "finished after {blocks} blocks");
        assert!(v.play());
    }

    #[test]
    fn getters_report_defaults() {
        let v = Voice::new(48000.0);
        assert_eq!(v.rate(), 1.0);
        assert_eq!(
            (v.loop_start(), v.loop_end(), v.looping()),
            (0.0, 0.0, false)
        );
        // Upstream's head init overrides the 0.01 s that reset sets.
        assert_eq!(v.fade_time(), 0.1);
        assert_eq!((v.rec_level(), v.pre_level()), (0.0, 0.0));
        assert!(!v.rec_once() && !v.rec() && !v.play());
        assert_eq!(v.rec_offset(), -8.0 / 48000.0);
        assert_eq!((v.rec_pre_slew_time(), v.rate_slew_time()), (0.001, 0.001));
        assert_eq!((v.phase_quant(), v.phase_offset()), (0.0, 0.0));
        assert_eq!(v.pre_filter_fc(), 16000.0);
        assert_eq!(v.pre_filter_rq(), 4.0);
        assert_eq!(
            [
                v.pre_filter_lp(),
                v.pre_filter_hp(),
                v.pre_filter_bp(),
                v.pre_filter_br()
            ],
            [1.0, 0.0, 0.0, 0.0]
        );
        assert_eq!((v.pre_filter_dry(), v.pre_filter_fc_mod()), (0.0, 1.0));
        assert_eq!(v.post_filter_fc(), 12000.0);
        assert_eq!(v.post_filter_rq(), 4.0);
        assert_eq!(
            [
                v.post_filter_lp(),
                v.post_filter_hp(),
                v.post_filter_bp(),
                v.post_filter_br()
            ],
            [0.0; 4]
        );
        assert_eq!(v.post_filter_dry(), 1.0);
    }

    #[test]
    fn getters_round_trip_every_setting() {
        use VoiceCmd::*;
        let mut v = Voice::new(44100.0);
        let cmds = [
            Rate(-1.5),
            LoopStart(0.25),
            LoopEnd(2.5),
            Loop(true),
            FadeTime(0.03),
            RecLevel(0.7),
            PreLevel(0.4),
            RecOffset(-0.001),
            RecPreSlewTime(0.2),
            RateSlewTime(0.3),
            PhaseQuant(0.125),
            PhaseOffset(0.5),
            PreFilterFc(3000.0),
            PreFilterRq(1.5),
            PreFilterLp(0.1),
            PreFilterHp(0.2),
            PreFilterBp(0.3),
            PreFilterBr(0.4),
            PreFilterDry(0.5),
            PreFilterFcMod(0.6),
            PostFilterFc(2000.0),
            PostFilterRq(2.5),
            PostFilterLp(0.15),
            PostFilterHp(0.25),
            PostFilterBp(0.35),
            PostFilterBr(0.45),
            PostFilterDry(0.55),
            Play(true),
            RecOnce(true),
        ];
        for c in cmds {
            v.apply(c);
        }
        let near = |a: f32, b: f32| (a - b).abs() < 1e-6;
        let got = [
            v.rate(),
            v.loop_start(),
            v.loop_end(),
            v.fade_time(),
            v.rec_level(),
            v.pre_level(),
            v.rec_pre_slew_time(),
            v.rate_slew_time(),
            v.phase_quant(),
            v.phase_offset(),
            v.pre_filter_fc(),
            v.pre_filter_rq(),
            v.pre_filter_lp(),
            v.pre_filter_hp(),
            v.pre_filter_bp(),
            v.pre_filter_br(),
            v.pre_filter_dry(),
            v.pre_filter_fc_mod(),
            v.post_filter_fc(),
            v.post_filter_rq(),
            v.post_filter_lp(),
            v.post_filter_hp(),
            v.post_filter_bp(),
            v.post_filter_br(),
            v.post_filter_dry(),
        ];
        let want = [
            -1.5, 0.25, 2.5, 0.03, 0.7, 0.4, 0.2, 0.3, 0.125, 0.5, 3000.0, 1.5, 0.1, 0.2, 0.3, 0.4,
            0.5, 0.6, 2000.0, 2.5, 0.15, 0.25, 0.35, 0.45, 0.55,
        ];
        for (k, (g, w)) in got.iter().zip(want).enumerate() {
            assert!(near(*g, w), "getter {k}: got {g}, want {w}");
        }
        assert!(v.looping() && v.play() && v.rec() && v.rec_once());
        // Stored as whole samples: -0.001 s at 44.1 kHz is -44 samples.
        assert_eq!(v.rec_offset(), -44.0 / 44100.0);
    }

    #[test]
    fn fixed_quirks_record_with_input_polarity_and_keep_short_fade() {
        for (quirks, sign, fade) in [(Quirks::Upstream, -1.0, 0.1), (Quirks::Fixed, 1.0, 0.01)] {
            let mut v = Voice::with_quirks(48000.0, quirks);
            assert_eq!(v.fade_time(), fade, "{quirks:?} after new");
            let mut buf = vec![0.0; 1 << 14];
            v.set_loop_end(0.2);
            v.set_loop(true);
            v.set_rec_level(1.0);
            v.set_rec(true);
            v.set_play(true);
            v.cut_to(0.0);
            let mut out = [0.0; 9000];
            v.process_block(&mut buf, &[0.25; 9000], &mut out);
            // Past the upstream 0.1 s crossfade, where only one head writes.
            // Soft clip gain is 1.2 below its knee, so 0.25 records as 0.3.
            let x = buf[7000];
            assert!((x - sign * 0.3).abs() < 1e-3, "{quirks:?}: recorded {x}");
            v.reset();
            assert_eq!(v.fade_time(), fade, "{quirks:?} after reset");
        }
    }

    #[test]
    fn post_filter_fc_reports_clamped_value() {
        let mut v = Voice::new(48000.0);
        v.set_post_filter_fc(1.0e6);
        assert_eq!(v.post_filter_fc(), 0.4 * 48000.0);
    }

    #[test]
    #[should_panic(expected = "power of two")]
    fn non_power_of_two_buffer_panics() {
        let mut v = Voice::new(48000.0);
        v.process_block(&mut [0.0; 1000], &[0.0; 4], &mut [0.0; 4]);
    }
}
