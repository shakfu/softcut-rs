//! Built-in effects for softcut, as a fixed-order, realtime-safe [`Chain`]:
//! saturation, bitcrusher, chorus, delay, reverb.
//!
//! A chain is mono or stereo, set at construction, and processes interleaved
//! frames in place. It allocates only in [`Chain::new`]. Settings change
//! through [`FxCmd`], a `Copy` enum, so a host can queue them to the audio
//! thread as it does softcut's own commands. Each effect has an on/off
//! switch; a disabled effect costs nothing and passes audio unchanged.
//!
//! Feedback paths flush values below 1e-20 to zero: a decaying tail would
//! otherwise reach denormal floats, which are slow on some x86 CPUs.

use std::f32::consts::TAU;

/// An effect in the chain, in processing order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fx {
    Saturation,
    Bitcrusher,
    Chorus,
    Delay,
    Reverb,
}

impl Fx {
    pub const ALL: [Fx; 5] = [
        Fx::Saturation,
        Fx::Bitcrusher,
        Fx::Chorus,
        Fx::Delay,
        Fx::Reverb,
    ];
}

/// A chain setting. Values are clamped to the ranges given; non-finite
/// values are ignored.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FxCmd {
    /// Turn an effect on or off. Turning one on clears its old tail.
    Enable(Fx, bool),
    /// Saturation input gain, 0..48 dB.
    Drive(f32),
    /// Saturation output gain, -48..12 dB.
    Level(f32),
    /// Bitcrusher resolution, 1..16 bits.
    Bits(f32),
    /// Bitcrusher sample-rate reduction factor, 1..64.
    Downsample(f32),
    /// Bitcrusher wet mix, 0..1.
    CrushMix(f32),
    /// Chorus modulation rate, 0.01..10 Hz.
    ChorusRate(f32),
    /// Chorus modulation depth, 0..10 ms.
    ChorusDepth(f32),
    ChorusMix(f32),
    /// Delay time, 0.001..2 s. Changes glide, like tape.
    DelayTime(f32),
    /// Delay feedback, 0..0.98.
    DelayFeedback(f32),
    /// Lowpass damping in the delay loop, 0 (none)..1.
    DelayDamp(f32),
    DelayMix(f32),
    /// Reverb room size, 0..1.
    ReverbSize(f32),
    /// Reverb high-frequency damping, 0..1.
    ReverbDamp(f32),
    /// Reverb stereo width, 0..1.
    ReverbWidth(f32),
    ReverbMix(f32),
    /// Silence every effect's tail.
    Clear,
}

const MAX_CHANNELS: usize = 2;

#[inline]
fn flush(x: f32) -> f32 {
    if x.abs() < 1e-20 { 0.0 } else { x }
}

fn db(x: f32) -> f32 {
    10f32.powf(x / 20.0)
}

/// Linear read `delay` frames (fractional) behind `write` in a power-of-two
/// ring. In f64: at f32 precision a position near 2^17 frames resolves only
/// 1/128 of a frame.
#[inline]
fn read(buf: &[[f32; MAX_CHANNELS]], write: usize, delay: f64, c: usize) -> f32 {
    let mask = buf.len() - 1;
    let pos = write as f64 - delay;
    let i = pos.floor();
    let frac = (pos - i) as f32;
    let i = (i as isize as usize) & mask;
    let (a, b) = (buf[i][c], buf[(i + 1) & mask][c]);
    a + (b - a) * frac
}

/// softcut's two-stage quadratic soft clipper, scaled to unity gain below its
/// knee.
struct Saturation {
    drive: f32,
    level: f32,
}

impl Saturation {
    const T: f32 = 0.68;
    const G: f32 = 1.2;
    const A: f32 = Self::G / (2.0 * (Self::T - 1.0));
    const B: f32 = Self::G * Self::T - Self::A * (Self::T - 1.0) * (Self::T - 1.0);

    fn clip(x: f32) -> f32 {
        let ax = x.abs().min(1.0);
        let y = if ax < Self::T {
            ax * Self::G
        } else {
            let q = ax - 1.0;
            Self::A * q * q + Self::B
        };
        x.signum() * y / Self::G
    }

    fn frame(&mut self, f: &mut [f32]) {
        for x in f {
            *x = Self::clip(*x * self.drive) * self.level;
        }
    }
}

struct Bitcrusher {
    bits: f32,
    downsample: f32,
    mix: f32,
    hold: [f32; MAX_CHANNELS],
    phase: f32,
}

impl Bitcrusher {
    fn frame(&mut self, f: &mut [f32]) {
        // Sample on the first frame of each hold period, so the output
        // starts at once rather than holding a stale value.
        let sample = self.phase <= 0.0;
        if sample {
            self.phase += self.downsample;
        }
        self.phase -= 1.0;
        let levels = 2f32.powf(self.bits - 1.0);
        for (c, x) in f.iter_mut().enumerate() {
            if sample {
                self.hold[c] = (*x * levels).round() / levels;
            }
            *x += (self.hold[c] - *x) * self.mix;
        }
    }

    fn clear(&mut self) {
        self.hold = [0.0; MAX_CHANNELS];
        self.phase = 0.0;
    }
}

struct Chorus {
    buf: Vec<[f32; MAX_CHANNELS]>,
    write: usize,
    sr: f32,
    phase: f32,
    rate: f32,
    depth_ms: f32,
    mix: f32,
}

impl Chorus {
    const BASE_MS: f32 = 12.0;

    fn frame(&mut self, f: &mut [f32]) {
        self.write = (self.write + 1) & (self.buf.len() - 1);
        for (c, &x) in f.iter().enumerate() {
            self.buf[self.write][c] = x;
        }
        for (c, x) in f.iter_mut().enumerate() {
            // Quadrature LFOs spread the two channels.
            let lfo = (TAU * (self.phase + c as f32 * 0.25)).sin();
            let ms = Self::BASE_MS + self.depth_ms * 0.5 * (1.0 + lfo);
            let wet = read(&self.buf, self.write, (ms * 0.001 * self.sr) as f64, c);
            *x += (wet - *x) * self.mix;
        }
        self.phase = (self.phase + self.rate / self.sr).fract();
    }

    fn clear(&mut self) {
        self.buf.fill([0.0; MAX_CHANNELS]);
    }
}

struct Delay {
    buf: Vec<[f32; MAX_CHANNELS]>,
    write: usize,
    sr: f32,
    /// Current delay in frames, gliding toward `target`. f64, because in f32
    /// the glide stalls short of the target: by ~0.04 frames at 10 ms, and by
    /// ~9 frames at 2 s.
    time: f64,
    target: f64,
    feedback: f32,
    damp: f32,
    lp: [f32; MAX_CHANNELS],
    mix: f32,
}

impl Delay {
    const MAX_SECONDS: f32 = 2.0;
    /// Per-frame glide coefficient: about 50 ms to settle.
    const GLIDE_SECONDS: f32 = 0.05;

    fn frame(&mut self, f: &mut [f32]) {
        let glide = (-1.0 / (Self::GLIDE_SECONDS as f64 * self.sr as f64)).exp();
        self.time = self.target + (self.time - self.target) * glide;
        // Land exactly: a residual fractional delay dulls the echo through
        // the linear interpolation.
        if (self.time - self.target).abs() < 1e-6 {
            self.time = self.target;
        }
        for (c, x) in f.iter_mut().enumerate() {
            let wet = read(&self.buf, self.write, self.time, c);
            self.lp[c] = flush(wet + (self.lp[c] - wet) * self.damp);
            self.buf[self.write][c] = flush(*x + self.lp[c] * self.feedback);
            *x += (wet - *x) * self.mix;
        }
        self.write = (self.write + 1) & (self.buf.len() - 1);
    }

    fn clear(&mut self) {
        self.buf.fill([0.0; MAX_CHANNELS]);
        self.lp = [0.0; MAX_CHANNELS];
    }
}

/// Freeverb's lowpass-feedback comb filter.
struct Comb {
    buf: Vec<f32>,
    i: usize,
    store: f32,
}

impl Comb {
    fn tick(&mut self, x: f32, feedback: f32, damp: f32) -> f32 {
        let y = self.buf[self.i];
        self.store = flush(y + (self.store - y) * damp);
        self.buf[self.i] = flush(x + self.store * feedback);
        self.i = (self.i + 1) % self.buf.len();
        y
    }
}

struct Allpass {
    buf: Vec<f32>,
    i: usize,
}

impl Allpass {
    fn tick(&mut self, x: f32) -> f32 {
        let b = self.buf[self.i];
        self.buf[self.i] = flush(x + b * 0.5);
        self.i = (self.i + 1) % self.buf.len();
        b - x
    }
}

/// Freeverb (Jezar at Dreampoint, public domain): 8 parallel combs into 4
/// series allpasses per channel, the right channel's delays 23 samples longer.
struct Reverb {
    combs: [Vec<Comb>; MAX_CHANNELS],
    allpasses: [Vec<Allpass>; MAX_CHANNELS],
    size: f32,
    damp: f32,
    width: f32,
    mix: f32,
}

impl Reverb {
    const COMBS: [usize; 8] = [1116, 1188, 1277, 1356, 1422, 1491, 1557, 1617];
    const ALLPASSES: [usize; 4] = [556, 441, 341, 225];
    const SPREAD: usize = 23;
    const INPUT_GAIN: f32 = 0.015;
    const WET_GAIN: f32 = 3.0;

    fn new(sr: f32) -> Self {
        // Freeverb's lengths are for 44.1 kHz.
        let len = |n: usize| ((n as f32 * sr / 44100.0).round() as usize).max(1);
        let spread = |c: usize| c * Self::SPREAD;
        Self {
            combs: std::array::from_fn(|c| {
                Self::COMBS
                    .iter()
                    .map(|&n| Comb {
                        buf: vec![0.0; len(n + spread(c))],
                        i: 0,
                        store: 0.0,
                    })
                    .collect()
            }),
            allpasses: std::array::from_fn(|c| {
                Self::ALLPASSES
                    .iter()
                    .map(|&n| Allpass {
                        buf: vec![0.0; len(n + spread(c))],
                        i: 0,
                    })
                    .collect()
            }),
            size: 0.7,
            damp: 0.5,
            width: 1.0,
            mix: 0.3,
        }
    }

    fn frame(&mut self, f: &mut [f32]) {
        let n = f.len();
        let input = f.iter().sum::<f32>() / n as f32 * Self::INPUT_GAIN;
        let feedback = self.size * 0.28 + 0.7;
        let damp = self.damp * 0.4;
        let mut out = [0.0f32; MAX_CHANNELS];
        for (c, o) in out.iter_mut().enumerate().take(n) {
            let mut y: f32 = self.combs[c]
                .iter_mut()
                .map(|comb| comb.tick(input, feedback, damp))
                .sum();
            for ap in &mut self.allpasses[c] {
                y = ap.tick(y);
            }
            *o = y * Self::WET_GAIN;
        }
        if n == 2 {
            let (w1, w2) = (0.5 + self.width * 0.5, (1.0 - self.width) * 0.5);
            out = [out[0] * w1 + out[1] * w2, out[1] * w1 + out[0] * w2];
        }
        for (x, wet) in f.iter_mut().zip(out) {
            *x += (wet - *x) * self.mix;
        }
    }

    fn clear(&mut self) {
        for comb in self.combs.iter_mut().flatten() {
            comb.buf.fill(0.0);
            comb.store = 0.0;
        }
        for ap in self.allpasses.iter_mut().flatten() {
            ap.buf.fill(0.0);
        }
    }
}

/// Saturation, bitcrusher, chorus, delay and reverb, in that order, each
/// switchable. All off at construction.
pub struct Chain {
    channels: usize,
    enabled: [bool; 5],
    saturation: Saturation,
    crusher: Bitcrusher,
    chorus: Chorus,
    delay: Delay,
    reverb: Reverb,
}

impl Chain {
    /// # Panics
    /// If `channels` is not 1 or 2.
    pub fn new(sample_rate: f32, channels: usize) -> Self {
        assert!(
            (1..=MAX_CHANNELS).contains(&channels),
            "a chain is mono or stereo"
        );
        let frames = |seconds: f32| ((seconds * sample_rate) as usize + 2).next_power_of_two();
        let delay_time = 0.375 * sample_rate as f64;
        Self {
            channels,
            enabled: [false; 5],
            saturation: Saturation {
                drive: db(6.0),
                level: db(-3.0),
            },
            crusher: Bitcrusher {
                bits: 8.0,
                downsample: 4.0,
                mix: 1.0,
                hold: [0.0; MAX_CHANNELS],
                phase: 0.0,
            },
            chorus: Chorus {
                buf: vec![[0.0; MAX_CHANNELS]; frames(0.03)],
                write: 0,
                sr: sample_rate,
                phase: 0.0,
                rate: 0.5,
                depth_ms: 3.0,
                mix: 0.5,
            },
            delay: Delay {
                buf: vec![[0.0; MAX_CHANNELS]; frames(Delay::MAX_SECONDS)],
                write: 0,
                sr: sample_rate,
                time: delay_time,
                target: delay_time,
                feedback: 0.4,
                damp: 0.3,
                lp: [0.0; MAX_CHANNELS],
                mix: 0.35,
            },
            reverb: Reverb::new(sample_rate),
        }
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    pub fn is_enabled(&self, fx: Fx) -> bool {
        self.enabled[fx as usize]
    }

    pub fn apply(&mut self, cmd: FxCmd) {
        use FxCmd::*;
        let value = match cmd {
            Enable(..) | Clear => 0.0,
            Drive(x) | Level(x) | Bits(x) | Downsample(x) | CrushMix(x) | ChorusRate(x)
            | ChorusDepth(x) | ChorusMix(x) | DelayTime(x) | DelayFeedback(x) | DelayDamp(x)
            | DelayMix(x) | ReverbSize(x) | ReverbDamp(x) | ReverbWidth(x) | ReverbMix(x) => x,
        };
        if !value.is_finite() {
            return;
        }
        let unit = value.clamp(0.0, 1.0);
        match cmd {
            Enable(fx, on) => {
                if on && !self.enabled[fx as usize] {
                    self.clear(fx);
                }
                self.enabled[fx as usize] = on;
            }
            Drive(x) => self.saturation.drive = db(x.clamp(0.0, 48.0)),
            Level(x) => self.saturation.level = db(x.clamp(-48.0, 12.0)),
            Bits(x) => self.crusher.bits = x.clamp(1.0, 16.0),
            Downsample(x) => self.crusher.downsample = x.clamp(1.0, 64.0),
            CrushMix(_) => self.crusher.mix = unit,
            ChorusRate(x) => self.chorus.rate = x.clamp(0.01, 10.0),
            ChorusDepth(x) => self.chorus.depth_ms = x.clamp(0.0, 10.0),
            ChorusMix(_) => self.chorus.mix = unit,
            DelayTime(x) => {
                self.delay.target = (x.clamp(0.001, Delay::MAX_SECONDS) * self.delay.sr) as f64
            }
            DelayFeedback(x) => self.delay.feedback = x.clamp(0.0, 0.98),
            DelayDamp(_) => self.delay.damp = unit,
            DelayMix(_) => self.delay.mix = unit,
            ReverbSize(_) => self.reverb.size = unit,
            ReverbDamp(_) => self.reverb.damp = unit,
            ReverbWidth(_) => self.reverb.width = unit,
            ReverbMix(_) => self.reverb.mix = unit,
            Clear => Fx::ALL.into_iter().for_each(|fx| self.clear(fx)),
        }
    }

    fn clear(&mut self, fx: Fx) {
        match fx {
            Fx::Saturation => {}
            Fx::Bitcrusher => self.crusher.clear(),
            Fx::Chorus => self.chorus.clear(),
            Fx::Delay => self.delay.clear(),
            Fx::Reverb => self.reverb.clear(),
        }
    }

    /// Process interleaved frames of `channels()` in place.
    ///
    /// # Panics
    /// If `buf` is not whole frames.
    pub fn process(&mut self, buf: &mut [f32]) {
        assert_eq!(buf.len() % self.channels, 0, "whole frames only");
        if !self.enabled.contains(&true) {
            return;
        }
        let on = self.enabled;
        for f in buf.chunks_exact_mut(self.channels) {
            if on[Fx::Saturation as usize] {
                self.saturation.frame(f);
            }
            if on[Fx::Bitcrusher as usize] {
                self.crusher.frame(f);
            }
            if on[Fx::Chorus as usize] {
                self.chorus.frame(f);
            }
            if on[Fx::Delay as usize] {
                self.delay.frame(f);
            }
            if on[Fx::Reverb as usize] {
                self.reverb.frame(f);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48000.0;

    fn only(fx: Fx, channels: usize) -> Chain {
        let mut c = Chain::new(SR, channels);
        c.apply(FxCmd::Enable(fx, true));
        c
    }

    fn impulse(n: usize, channels: usize) -> Vec<f32> {
        let mut x = vec![0.0; n * channels];
        x[..channels].fill(1.0);
        x
    }

    fn peak(x: &[f32]) -> f32 {
        x.iter().fold(0.0, |m, v| m.max(v.abs()))
    }

    fn sine(n: usize, hz: f32, amp: f32) -> Vec<f32> {
        (0..n)
            .map(|i| amp * (TAU * hz * i as f32 / SR).sin())
            .collect()
    }

    #[test]
    fn a_chain_with_everything_off_is_the_identity() {
        let x = sine(4800, 440.0, 0.5);
        let mut y = x.clone();
        Chain::new(SR, 1).process(&mut y);
        assert_eq!(x, y);
    }

    #[test]
    fn saturation_is_unity_below_the_knee_and_bounded_above() {
        let mut c = only(Fx::Saturation, 1);
        c.apply(FxCmd::Drive(0.0));
        c.apply(FxCmd::Level(0.0));
        let mut y = vec![0.1, -0.3, 0.5];
        c.process(&mut y);
        assert!(
            y.iter()
                .zip([0.1, -0.3, 0.5])
                .all(|(a, b)| (a - b).abs() < 1e-6),
            "{y:?}"
        );
        c.apply(FxCmd::Drive(40.0));
        let mut loud = sine(4800, 100.0, 1.0);
        c.process(&mut loud);
        assert!(peak(&loud) <= 0.85, "{}", peak(&loud));
    }

    #[test]
    fn bitcrusher_quantizes_and_holds() {
        let mut c = only(Fx::Bitcrusher, 1);
        c.apply(FxCmd::Bits(2.0));
        c.apply(FxCmd::Downsample(4.0));
        let mut y = sine(4800, 50.0, 0.9);
        c.process(&mut y);
        // 2 bits: steps of 0.5.
        assert!(y.iter().all(|v| (v * 2.0).fract() == 0.0), "not quantized");
        // Held for 4 frames at a time.
        assert!(
            y.chunks(4)
                .skip(1)
                .take(100)
                .all(|w| w.iter().all(|&v| v == w[0]))
        );
    }

    #[test]
    fn delay_repeats_at_its_time_with_feedback() {
        let mut c = only(Fx::Delay, 2);
        c.apply(FxCmd::DelayTime(0.01));
        c.apply(FxCmd::DelayFeedback(0.5));
        c.apply(FxCmd::DelayDamp(0.0));
        c.apply(FxCmd::DelayMix(1.0));
        // Let the time glide settle before the impulse.
        let mut settle = vec![0.0; 48000 * 2];
        c.process(&mut settle);
        let mut y = impulse(2000, 2);
        c.process(&mut y);
        let left: Vec<f32> = y.chunks(2).map(|f| f[0]).collect();
        let d = (0.01 * SR) as usize;
        assert!((left[d] - 1.0).abs() < 1e-3, "first echo {}", left[d]);
        assert!(
            (left[2 * d] - 0.5).abs() < 1e-3,
            "second echo {}",
            left[2 * d]
        );
        assert!(left[d / 2].abs() < 1e-6);
    }

    /// A long delay glides onto its time exactly: in f32 it would stall
    /// frames short and blur the echo.
    #[test]
    fn long_delay_lands_on_its_time() {
        let mut c = only(Fx::Delay, 1);
        c.apply(FxCmd::DelayTime(1.5));
        c.apply(FxCmd::DelayFeedback(0.0));
        c.apply(FxCmd::DelayMix(1.0));
        c.apply(FxCmd::DelayDamp(0.0));
        let mut settle = vec![0.0; 48000];
        c.process(&mut settle);
        let mut y = impulse(80000, 1);
        c.process(&mut y);
        let d = (1.5 * SR) as usize;
        assert_eq!(
            y[d],
            1.0,
            "echo {} at {d}; neighbours {} {}",
            y[d],
            y[d - 1],
            y[d + 1]
        );
    }

    #[test]
    fn reverb_rings_decays_and_never_denormalizes() {
        let mut c = only(Fx::Reverb, 2);
        c.apply(FxCmd::ReverbMix(1.0));
        let mut y = impulse(SR as usize * 30, 2);
        c.process(&mut y);
        let second = |s: usize| &y[s * SR as usize * 2..(s + 1) * SR as usize * 2];
        assert!(peak(second(0)) > 0.01, "no reverb");
        assert!(peak(second(1)) < peak(second(0)), "not decaying");
        assert!(y.iter().all(|v| v.is_finite()));
        // The tail has fully decayed to exact zeros, never denormals.
        assert!(
            second(29)
                .iter()
                .all(|&v| v == 0.0 || v.abs() >= f32::MIN_POSITIVE)
        );
    }

    #[test]
    fn chorus_mix_zero_is_dry_and_output_stays_bounded() {
        let mut c = only(Fx::Chorus, 2);
        c.apply(FxCmd::ChorusMix(0.0));
        let x: Vec<f32> = sine(4800, 440.0, 0.5)
            .into_iter()
            .flat_map(|v| [v, v])
            .collect();
        let mut y = x.clone();
        c.process(&mut y);
        assert_eq!(x, y);
        c.apply(FxCmd::ChorusMix(1.0));
        c.apply(FxCmd::ChorusDepth(10.0));
        c.apply(FxCmd::ChorusRate(5.0));
        c.process(&mut y);
        assert!(peak(&y) <= 0.5 + 1e-3);
    }

    #[test]
    fn enabling_clears_old_tails_and_bad_values_are_ignored() {
        let mut c = only(Fx::Delay, 1);
        c.apply(FxCmd::DelayMix(1.0));
        let mut y = impulse(100, 1);
        c.process(&mut y);
        c.apply(FxCmd::Enable(Fx::Delay, false));
        c.apply(FxCmd::Enable(Fx::Delay, true));
        let mut silent = vec![0.0; 48000];
        c.process(&mut silent);
        assert!(silent.iter().all(|&v| v == 0.0), "stale delay tail");

        c.apply(FxCmd::DelayFeedback(f32::NAN));
        c.apply(FxCmd::DelayTime(f32::INFINITY));
        let mut x = sine(4800, 440.0, 0.5);
        c.process(&mut x);
        assert!(x.iter().all(|v| v.is_finite()));
    }
}
