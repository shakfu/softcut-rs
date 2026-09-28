//! Leaf DSP primitives: interpolation, fade curves, soft clipper, slew ramp.
//!
//! Ported from softcut-lib's `Interpolate.h`, `FadeCurves.cpp`, `SoftClip.h`
//! and `Utilities.h`. Arithmetic keeps the C++ precision (f32 vs f64) of each
//! expression so output tracks the reference implementation.

/// 4-point, 3rd-order Hermite (x-form) over f32 samples.
///
/// The C++ template instantiates with `T = float`, but its constants are double
/// literals, so most of the expression is evaluated in f64. Reproduced here.
#[inline]
pub(crate) fn hermite_f32(x: f32, y0: f32, y1: f32, y2: f32, y3: f32) -> f32 {
    let x = x as f64;
    let c3 = 0.5 * (y3 - y0) as f64 + 1.5 * (y1 - y2) as f64;
    let c2 = y0 as f64 - 2.5 * y1 as f64 + 2.0 * y2 as f64 - 0.5 * y3 as f64;
    let c1 = 0.5 * (y2 - y0) as f64;
    (((c3 * x + c2) * x + c1) * x + y1 as f64) as f32
}

/// 4-point, 3rd-order Hermite (x-form) in f64.
#[inline]
pub(crate) fn hermite_f64(x: f64, y0: f64, y1: f64, y2: f64, y3: f64) -> f64 {
    (((0.5 * (y3 - y0) + 1.5 * (y1 - y2)) * x + (y0 - 2.5 * y1 + 2.0 * y2 - 0.5 * y3)) * x
        + 0.5 * (y2 - y0))
        * x
        + y1
}

/// `x > 0 ? 1 : -1`; zero maps to -1, as in softcut.
#[inline]
pub(crate) fn fsign(x: f32) -> f32 {
    if x > 0.0 { 1.0 } else { -1.0 }
}

use crate::Quirks;
const FADE_BUF_SIZE: usize = 1001;
const FPI: f32 = std::f32::consts::PI;

/// Shape of a fade curve. Port of softcut-lib's `FadeCurves::Shape`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FadeShape {
    #[default]
    Linear,
    /// Half a cosine: slow at both ends.
    Sine,
    /// A quarter sine: fast at the start, slow at the end.
    Raised,
}

/// Pre/rec level curves applied across a subhead's crossfade, as 1001-point
/// tables over the fade's progress. Port of `FadeCurves.cpp`.
///
/// The rec curve scales new input: 0 for the first `rec_delay_ratio` of the
/// fade, then rising to 1. The pre curve lifts the pre level towards 1 (keep
/// the old content) over the first `pre_window_ratio`, then 0.
#[derive(Clone)]
pub(crate) struct FadeCurves {
    rec: [f32; FADE_BUF_SIZE],
    pre: [f32; FADE_BUF_SIZE],
    rec_shape: FadeShape,
    pre_shape: FadeShape,
    rec_delay_ratio: f32,
    pre_window_ratio: f32,
    quirks: Quirks,
}

impl FadeCurves {
    /// softcut's defaults: linear pre window of 1/8, raised rec curve delayed
    /// by 1/128.
    pub(crate) fn new(quirks: Quirks) -> Self {
        let mut c = Self {
            rec: [0.0; FADE_BUF_SIZE],
            pre: [0.0; FADE_BUF_SIZE],
            rec_shape: FadeShape::Raised,
            pre_shape: FadeShape::Linear,
            rec_delay_ratio: 1.0 / 128.0,
            pre_window_ratio: 1.0 / 8.0,
            quirks,
        };
        c.calc_rec();
        c.calc_pre();
        c
    }

    pub(crate) fn set_rec_shape(&mut self, shape: FadeShape) {
        self.rec_shape = shape;
        self.calc_rec();
    }

    pub(crate) fn set_pre_shape(&mut self, shape: FadeShape) {
        self.pre_shape = shape;
        self.calc_pre();
    }

    pub(crate) fn set_rec_delay_ratio(&mut self, x: f32) {
        self.rec_delay_ratio = x;
        self.calc_rec();
    }

    pub(crate) fn set_pre_window_ratio(&mut self, x: f32) {
        self.pre_window_ratio = x;
        self.calc_pre();
    }

    pub(crate) fn rec_shape(&self) -> FadeShape {
        self.rec_shape
    }

    pub(crate) fn pre_shape(&self) -> FadeShape {
        self.pre_shape
    }

    pub(crate) fn rec_delay_ratio(&self) -> f32 {
        self.rec_delay_ratio
    }

    pub(crate) fn pre_window_ratio(&self) -> f32 {
        self.pre_window_ratio
    }

    // Table points before the curve starts. Upstream overruns its table for
    // ratios above 1; clamped here.
    fn calc_rec(&mut self) {
        let n = FADE_BUF_SIZE - 1;
        let ndr = ((self.rec_delay_ratio * FADE_BUF_SIZE as f32) as usize).min(n);
        let nr = n - ndr;
        let buf = &mut self.rec;
        buf[..ndr].fill(0.0);
        let curve = &mut buf[ndr..n];
        match self.rec_shape {
            FadeShape::Sine => {
                let phi = FPI / nr as f32;
                let mut x = FPI;
                for v in curve {
                    *v = x.cos() * 0.5 + 0.5;
                    x += phi;
                }
            }
            FadeShape::Linear => {
                let phi = 1.0 / nr as f32;
                let mut x = 0.0f32;
                for v in curve {
                    *v = x;
                    x += phi;
                }
            }
            FadeShape::Raised => {
                // NB: upstream computes `-sin(x)`, so the curve runs 0 -> -1
                // and recorded material is polarity-inverted.
                let sign = match self.quirks {
                    Quirks::Upstream => -1.0,
                    Quirks::Fixed => 1.0,
                };
                let phi = FPI / (nr * 2) as f32;
                let mut x = 0.0f32;
                for v in curve {
                    *v = sign * x.sin();
                    x += phi;
                }
            }
        }
        buf[n] = 1.0;
    }

    fn calc_pre(&mut self) {
        // NB: upstream tests the rec shape where it means the pre shape, so a
        // raised pre curve only applies while the rec curve is also raised;
        // otherwise the table keeps its previous contents.
        if self.pre_shape == FadeShape::Raised
            && self.quirks == Quirks::Upstream
            && self.rec_shape != FadeShape::Raised
        {
            return;
        }
        let nwp = ((self.pre_window_ratio * FADE_BUF_SIZE as f32) as usize).min(FADE_BUF_SIZE);
        let buf = &mut self.pre;
        let window = &mut buf[..nwp];
        let mut x = 0.0f32;
        match self.pre_shape {
            FadeShape::Sine => {
                let phi = FPI / nwp as f32;
                for v in window {
                    *v = x.cos() * 0.5 + 0.5;
                    x += phi;
                }
            }
            FadeShape::Linear => {
                let phi = 1.0 / nwp as f32;
                for v in window {
                    *v = 1.0 - x;
                    x += phi;
                }
            }
            FadeShape::Raised => {
                let phi = FPI / (nwp * 2) as f32;
                for v in window {
                    *v = x.cos();
                    x += phi;
                }
            }
        }
        buf[nwp..].fill(0.0);
    }

    #[inline]
    pub(crate) fn rec_value(&self, x: f32) -> f32 {
        tab_linear(&self.rec, x)
    }

    #[inline]
    pub(crate) fn pre_value(&self, x: f32) -> f32 {
        tab_linear(&self.pre, x)
    }
}

/// Linear table lookup for `x` in [0, 1]. Scales by `N - 2`, as upstream does,
/// so the last table entry is never reached.
#[inline]
fn tab_linear(buf: &[f32; FADE_BUF_SIZE], x: f32) -> f32 {
    let fi = x * (FADE_BUF_SIZE - 2) as f32;
    let i = (fi as usize).min(FADE_BUF_SIZE - 2);
    let (a, b) = (buf[i], buf[i + 1]);
    a + (fi - i as f32) * (b - a)
}

/// Two-stage quadratic soft clipper, fixed at softcut's threshold and gain.
pub(crate) struct SoftClip;

impl SoftClip {
    const T: f32 = 0.68;
    const G: f32 = 1.2;
    const A: f32 = Self::G / (2.0 * (Self::T - 1.0));
    const B: f32 = Self::G * Self::T - (Self::A * (Self::T - 1.0) * (Self::T - 1.0));

    #[inline]
    pub(crate) fn process(x: f32) -> f32 {
        let ax = x.abs().min(1.0);
        if ax < Self::T {
            x * Self::G
        } else {
            let q = ax - 1.0;
            fsign(x) * (Self::A * q * q + Self::B)
        }
    }
}

/// One-pole smoother; `time` is the time to converge within -60 dB.
#[derive(Clone)]
pub(crate) struct LogRamp {
    sample_rate: f32,
    time: f32,
    b: f32,
    x0: f32,
    y0: f32,
}

impl LogRamp {
    pub(crate) fn new(sample_rate: f32, time: f32) -> Self {
        let mut r = Self {
            sample_rate,
            time,
            b: 1.0,
            x0: 0.0,
            y0: 0.0,
        };
        r.set_time(time);
        r
    }

    pub(crate) fn set_sample_rate(&mut self, sr: f32) {
        self.sample_rate = sr;
        self.set_time(self.time);
    }

    pub(crate) fn set_time(&mut self, t: f32) {
        self.time = t;
        self.b = (-6.9f32 / (t * self.sample_rate)).exp();
    }

    pub(crate) fn target(&self) -> f32 {
        self.x0
    }

    pub(crate) fn time(&self) -> f32 {
        self.time
    }

    pub(crate) fn set_target(&mut self, x: f32) {
        self.x0 = x;
    }

    #[inline]
    pub(crate) fn update(&mut self) -> f32 {
        self.y0 = self.x0 + (self.y0 - self.x0) * self.b;
        self.y0
    }

    pub(crate) fn reset(&mut self, x: f32) {
        self.x0 = x;
        self.y0 = x;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn soft_clip_is_continuous_at_knee_and_saturates() {
        let below = SoftClip::process(SoftClip::T - 1e-6);
        let above = SoftClip::process(SoftClip::T + 1e-6);
        assert!((below - above).abs() < 1e-4);
        assert!((SoftClip::process(1.0) - SoftClip::B).abs() < 1e-6);
        assert_eq!(SoftClip::process(5.0), SoftClip::process(1.0));
        assert_eq!(SoftClip::process(-0.5), -SoftClip::process(0.5));
    }

    #[test]
    fn fade_curves_endpoints() {
        let c = FadeCurves::new(Quirks::Upstream);
        assert_eq!(c.pre_value(0.0), 1.0);
        assert_eq!(c.pre_value(1.0), 0.0);
        assert_eq!(c.rec_value(0.0), 0.0);
        assert!((c.rec_value(1.0) + 1.0).abs() < 1e-3);
        let fixed = FadeCurves::new(Quirks::Fixed);
        assert!((fixed.rec_value(1.0) - 1.0).abs() < 1e-3);
    }

    fn monotonic(xs: &[f32], rising: bool) -> bool {
        xs.windows(2)
            .all(|w| if rising { w[1] >= w[0] } else { w[1] <= w[0] })
    }

    #[test]
    fn every_shape_spans_its_range() {
        for shape in [FadeShape::Linear, FadeShape::Sine, FadeShape::Raised] {
            let mut c = FadeCurves::new(Quirks::Fixed);
            c.set_rec_shape(shape);
            c.set_pre_shape(shape);
            let n = FADE_BUF_SIZE - 1;
            let ndr = ((1.0f32 / 128.0) * FADE_BUF_SIZE as f32) as usize;
            assert!(
                c.rec[..ndr].iter().all(|&x| x == 0.0),
                "{shape:?} rec delay"
            );
            assert!(monotonic(&c.rec[..n], true), "{shape:?} rec rises");
            assert!(
                c.rec[n - 1] > 0.99,
                "{shape:?} rec reaches {}",
                c.rec[n - 1]
            );
            let nwp = FADE_BUF_SIZE / 8;
            assert_eq!(c.pre[0], 1.0, "{shape:?} pre starts at 1");
            assert!(monotonic(&c.pre, false), "{shape:?} pre falls");
            assert!(
                c.pre[nwp..].iter().all(|&x| x == 0.0),
                "{shape:?} pre window"
            );
        }
    }

    #[test]
    fn ratios_move_the_curves_and_clamp() {
        let mut c = FadeCurves::new(Quirks::Fixed);
        c.set_rec_delay_ratio(0.5);
        assert_eq!(c.rec[499], 0.0);
        assert!(c.rec[520] > 0.0);
        c.set_pre_window_ratio(0.25);
        assert!(c.pre[240] > 0.0 && c.pre[251] == 0.0);
        // Upstream overruns its table past 1; here both clamp.
        c.set_rec_delay_ratio(3.0);
        c.set_pre_window_ratio(3.0);
        assert!(c.rec[..FADE_BUF_SIZE - 1].iter().all(|&x| x == 0.0));
        assert_eq!(c.rec[FADE_BUF_SIZE - 1], 1.0);
        // The window covers the whole table, so it never reaches 0.
        let last = c.pre[FADE_BUF_SIZE - 1];
        assert!(last > 0.0 && last < 0.01, "{last}");
    }

    #[test]
    fn raised_pre_shape_follows_upstream_bug_only_with_upstream_quirks() {
        for (quirks, applies) in [(Quirks::Upstream, false), (Quirks::Fixed, true)] {
            let mut c = FadeCurves::new(quirks);
            c.set_rec_shape(FadeShape::Linear);
            let before = c.pre;
            c.set_pre_shape(FadeShape::Raised);
            assert_eq!(c.pre != before, applies, "{quirks:?}");
        }
        // With the rec curve raised, upstream applies it too.
        let mut c = FadeCurves::new(Quirks::Upstream);
        let before = c.pre;
        c.set_pre_shape(FadeShape::Raised);
        assert_ne!(c.pre, before);
    }

    #[test]
    fn hermite_passes_through_y1_at_zero() {
        assert_eq!(hermite_f32(0.0, 3.0, 0.25, -1.0, 2.0), 0.25);
        assert_eq!(hermite_f64(0.0, 3.0, 0.25, -1.0, 2.0), 0.25);
    }
}
