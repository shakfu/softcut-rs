//! Crossfaded read/write head. Port of `SubHead.cpp` and `ReadWriteHead.cpp`.
//!
//! The buffer is not stored: every processing call borrows it, together with
//! `mask = len - 1` (length is a power of two). Write indices are therefore
//! kept unwrapped until use, since a cut can happen with no buffer in hand.

use crate::dsp::{FadeCurves, SoftClip, fsign, hermite_f32};
use crate::resampler::Resampler;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum State {
    Playing,
    Stopped,
    FadeIn,
    FadeOut,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Action {
    None,
    Stop,
    LoopPos,
    LoopNeg,
}

#[inline]
fn wrap(x: i64, mask: usize) -> usize {
    (x as usize) & mask
}

/// One half of the crossfaded head.
#[derive(Clone)]
struct SubHead {
    resamp: Resampler,
    wr_idx: i64,
    state: State,
    rate: f64,
    inc_dir: i64,
    phase: f64,
    fade: f32,
    active: bool,
    rec_offset: i64,
}

impl SubHead {
    fn new() -> Self {
        Self {
            resamp: Resampler::new(),
            wr_idx: 0,
            state: State::Stopped,
            rate: 1.0,
            inc_dir: 1,
            phase: 0.0,
            fade: 0.0,
            active: false,
            rec_offset: -8,
        }
    }

    fn init(&mut self) {
        self.phase = 0.0;
        self.fade = 0.0;
        self.state = State::Stopped;
        self.resamp.set_phase(0.0);
        self.inc_dir = 1;
        self.rec_offset = -8;
    }

    fn update_phase(&mut self, start: f64, end: f64, looping: bool) -> Action {
        if self.state == State::Stopped {
            return Action::None;
        }
        let p = self.phase + self.rate;
        let mut res = Action::None;
        if self.active && (p > end || p < start) {
            if looping {
                res = if self.rate > 0.0 {
                    Action::LoopPos
                } else {
                    Action::LoopNeg
                };
            } else {
                self.state = State::FadeOut;
                res = Action::Stop;
            }
        }
        self.phase = p;
        res
    }

    fn update_fade(&mut self, inc: f32) {
        match self.state {
            State::FadeIn => {
                self.fade += inc;
                if self.fade > 1.0 {
                    self.fade = 1.0;
                    self.state = State::Playing;
                }
            }
            State::FadeOut => {
                self.fade -= inc;
                if self.fade < 0.0 {
                    self.fade = 0.0;
                    self.state = State::Stopped;
                }
            }
            State::Playing | State::Stopped => {}
        }
    }

    #[inline]
    fn poke(
        &mut self,
        x: f32,
        pre: f32,
        rec: f32,
        curves: &FadeCurves,
        buf: &mut [f32],
        mask: usize,
    ) {
        // The resampler always consumes input, so its history stays continuous.
        let nframes = self.resamp.process_frame(x);
        if self.state == State::Stopped {
            return;
        }
        let pre_fade = pre + (1.0 - pre) * curves.pre_value(self.fade);
        let rec_fade = rec * curves.rec_value(self.fade);
        let mut idx = wrap(self.wr_idx, mask);
        for &y in &self.resamp.output()[..nframes] {
            let y = SoftClip::process(y);
            buf[idx] *= pre_fade;
            buf[idx] += y * rec_fade;
            idx = wrap(idx as i64 + self.inc_dir, mask);
        }
        self.wr_idx = idx as i64;
    }

    #[inline]
    fn peek(&self, buf: &[f32], mask: usize) -> f32 {
        let p1 = self.phase as i32 as i64;
        let y0 = buf[wrap(p1 - 1, mask)];
        let y1 = buf[wrap(p1, mask)];
        let y2 = buf[wrap(p1 + 1, mask)];
        let y3 = buf[wrap(p1 + 2, mask)];
        let x = (self.phase - p1 as f32 as f64) as f32;
        hermite_f32(x, y0, y1, y2, y3)
    }

    fn set_phase(&mut self, phase: f64) {
        self.phase = phase;
        self.wr_idx = phase as i32 as i64 + self.inc_dir * self.rec_offset;
    }

    fn set_rate(&mut self, rate: f64) {
        self.rate = rate;
        self.inc_dir = fsign(rate as f32) as i64;
        // The resampler does not handle negative rates; the subhead writes
        // backwards instead.
        self.resamp.set_rate(rate.abs());
    }

    fn set_state(&mut self, state: State) {
        self.state = state;
        match state {
            State::Stopped => self.fade = 0.0,
            State::Playing => self.fade = 1.0,
            _ => {}
        }
    }
}

/// Two subheads, crossfaded on every cut and loop wrap.
#[derive(Clone)]
pub(crate) struct ReadWriteHead {
    head: [SubHead; 2],
    curves: FadeCurves,
    sr: f32,
    start: f64,
    end: f64,
    queued_crossfade: f64,
    queued_crossfade_flag: bool,
    fade_time: f32,
    fade_inc: f32,
    active: usize,
    loop_flag: bool,
    pre: f32,
    rec: f32,
    rec_once_flag: bool,
    rec_once_done: bool,
    rec_once_head: Option<usize>,
    rate: f64,
}

impl ReadWriteHead {
    pub(crate) fn new(quirks: crate::Quirks) -> Self {
        let mut h = Self {
            head: [SubHead::new(), SubHead::new()],
            curves: FadeCurves::new(quirks),
            sr: 48000.0,
            start: 0.0,
            end: 0.0,
            queued_crossfade: 0.0,
            queued_crossfade_flag: false,
            fade_time: 0.1,
            fade_inc: 0.0,
            active: 0,
            loop_flag: false,
            pre: 0.0,
            rec: 0.0,
            rec_once_flag: false,
            rec_once_done: false,
            rec_once_head: None,
            rate: 1.0,
        };
        h.init();
        h
    }

    /// Resets transport state. Keeps sample rate, loop flag and levels.
    pub(crate) fn init(&mut self) {
        self.start = 0.0;
        self.end = 0.0;
        self.active = 0;
        self.rate = 1.0;
        self.set_fade_time(0.1);
        self.queued_crossfade = 0.0;
        self.queued_crossfade_flag = false;
        self.head[0].init();
        self.head[1].init();
        self.set_rec_once_flag(false);
    }

    #[inline]
    fn mix_fade(&self, buf: &[f32], mask: usize) -> f32 {
        const HALF_PI: f32 = std::f32::consts::FRAC_PI_2;
        let [h0, h1] = &self.head;
        h0.peek(buf, mask) * (h0.fade * HALF_PI).sin()
            + h1.peek(buf, mask) * (h1.fade * HALF_PI).sin()
    }

    #[inline]
    fn poke(&mut self, x: f32, buf: &mut [f32], mask: usize) {
        let (pre, rec) = (self.pre, self.rec);
        let curves = &self.curves;
        if self.rec_once_flag || self.rec_once_done || self.rec_once_head.is_some() {
            if let Some(h) = self.rec_once_head {
                self.head[h].poke(x, pre, rec, curves, buf, mask);
            }
        } else {
            self.head[0].poke(x, pre, rec, curves, buf, mask);
            self.head[1].poke(x, pre, rec, curves, buf, mask);
        }
    }

    #[inline]
    fn advance(&mut self) {
        for i in 0..2 {
            let act = self.head[i].update_phase(self.start, self.end, self.loop_flag);
            self.take_action(act);
        }
        self.head[0].update_fade(self.fade_inc);
        self.head[1].update_fade(self.fade_inc);
        self.dequeue_crossfade();
    }

    /// Read and write.
    #[inline]
    pub(crate) fn process_sample(&mut self, x: f32, buf: &mut [f32], mask: usize) -> f32 {
        let y = self.mix_fade(buf, mask);
        self.poke(x, buf, mask);
        self.advance();
        y
    }

    /// Write only. Upstream leaves the output unwritten; this returns 0.
    #[inline]
    pub(crate) fn process_sample_no_read(&mut self, x: f32, buf: &mut [f32], mask: usize) -> f32 {
        self.poke(x, buf, mask);
        self.advance();
        0.0
    }

    /// Read only.
    #[inline]
    pub(crate) fn process_sample_no_write(&mut self, buf: &[f32], mask: usize) -> f32 {
        let y = self.mix_fade(buf, mask);
        self.advance();
        y
    }

    pub(crate) fn set_rate(&mut self, x: f64) {
        self.rate = x;
        self.calc_fade_inc();
        self.head[0].set_rate(x);
        self.head[1].set_rate(x);
    }

    pub(crate) fn set_loop_start_seconds(&mut self, x: f32) {
        self.start = (x * self.sr) as f64;
        self.queued_crossfade_flag = false;
    }

    pub(crate) fn set_loop_end_seconds(&mut self, x: f32) {
        self.end = (x * self.sr) as f64;
        self.queued_crossfade_flag = false;
    }

    fn take_action(&mut self, act: Action) {
        match act {
            Action::LoopPos => self.enqueue_crossfade(self.start),
            Action::LoopNeg => self.enqueue_crossfade(self.end),
            Action::Stop | Action::None => {}
        }
    }

    fn enqueue_crossfade(&mut self, pos: f64) {
        self.queued_crossfade = pos;
        self.queued_crossfade_flag = true;
    }

    fn is_fading(&self) -> bool {
        matches!(self.head[self.active].state, State::FadeIn | State::FadeOut)
    }

    fn dequeue_crossfade(&mut self) {
        if !self.is_fading() {
            if self.queued_crossfade_flag {
                self.cut_to_phase(self.queued_crossfade);
            }
            self.queued_crossfade_flag = false;
        }
    }

    fn cut_to_phase(&mut self, pos: f64) {
        if self.is_fading() {
            // Unreachable from the public API; upstream logs and returns.
            return;
        }
        let s = self.head[self.active].state;
        let new_active = self.active ^ 1;
        if s != State::Stopped {
            self.head[self.active].set_state(State::FadeOut);
        }
        if self.rec_once_head == Some(new_active) {
            self.rec_once_head = None;
            self.rec_once_done = true;
        }
        if self.rec_once_flag {
            self.rec_once_flag = false;
            self.rec_once_head = Some(new_active);
        }
        self.head[new_active].set_state(State::FadeIn);
        self.head[new_active].set_phase(pos);
        self.head[self.active].active = false;
        self.head[new_active].active = true;
        self.active = new_active;
    }

    pub(crate) fn set_fade_time(&mut self, secs: f32) {
        self.fade_time = secs;
        self.calc_fade_inc();
    }

    fn calc_fade_inc(&mut self) {
        let inc = (self.rate.abs() as f32) / (self.fade_time * self.sr).max(1.0);
        self.fade_inc = inc.clamp(0.0, 1.0);
    }

    pub(crate) fn set_loop_flag(&mut self, val: bool) {
        self.loop_flag = val;
    }

    pub(crate) fn set_rec_once_flag(&mut self, val: bool) {
        self.rec_once_flag = val;
        self.rec_once_done = false;
        self.rec_once_head = None;
    }

    pub(crate) fn rec_once_done(&self) -> bool {
        self.rec_once_done
    }

    pub(crate) fn rec_once_active(&self) -> bool {
        self.rec_once_done || self.rec_once_flag || self.rec_once_head.is_some()
    }

    pub(crate) fn set_sample_rate(&mut self, sr: f32) {
        self.sr = sr;
    }

    pub(crate) fn set_rec(&mut self, x: f32) {
        self.rec = x;
    }

    pub(crate) fn set_pre(&mut self, x: f32) {
        self.pre = x;
    }

    pub(crate) fn active_phase(&self) -> f64 {
        self.head[self.active].phase
    }

    /// Cut with crossfade; queued if a crossfade is already running.
    pub(crate) fn cut_to_pos(&mut self, seconds: f32) {
        let pos = (seconds * self.sr) as f64;
        if self.is_fading() {
            self.enqueue_crossfade(pos);
        } else {
            self.cut_to_phase(pos);
        }
    }

    pub(crate) fn rate(&self) -> f64 {
        self.rate
    }

    pub(crate) fn curves(&self) -> &FadeCurves {
        &self.curves
    }

    pub(crate) fn curves_mut(&mut self) -> &mut FadeCurves {
        &mut self.curves
    }

    pub(crate) fn loop_start_seconds(&self) -> f32 {
        (self.start / self.sr as f64) as f32
    }

    pub(crate) fn loop_end_seconds(&self) -> f32 {
        (self.end / self.sr as f64) as f32
    }

    pub(crate) fn loop_flag(&self) -> bool {
        self.loop_flag
    }

    pub(crate) fn fade_time(&self) -> f32 {
        self.fade_time
    }

    pub(crate) fn rec_offset_samples(&self) -> i64 {
        self.head[0].rec_offset
    }

    pub(crate) fn set_rec_offset_samples(&mut self, d: i32) {
        self.head[0].rec_offset = d as i64;
        self.head[1].rec_offset = d as i64;
    }

    pub(crate) fn stop(&mut self) {
        self.head[0].set_state(State::Stopped);
        self.head[1].set_state(State::Stopped);
    }

    /// Start playing from the current position without a fade-in.
    pub(crate) fn run(&mut self) {
        self.head[self.active].set_state(State::Playing);
    }
}
