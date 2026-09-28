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

const FADE_BUF_SIZE: usize = 1001;
const FPI: f32 = std::f32::consts::PI;

/// Pre/rec level curves applied across a subhead's crossfade.
///
/// Only softcut's defaults are built (linear pre window of 1/8, raised rec
/// curve delayed by 1/128): `softcut::Voice` exposes no setter for the others.
#[derive(Clone)]
pub(crate) struct FadeCurves {
    rec: [f32; FADE_BUF_SIZE],
    pre: [f32; FADE_BUF_SIZE],
}

impl FadeCurves {
    pub(crate) fn new() -> Self {
        let n = FADE_BUF_SIZE - 1;

        // Rec curve, "raised" shape. NB: upstream computes `-sin(x)`, so the
        // curve runs 0 -> -1 and recorded material is polarity-inverted.
        let mut rec = [0.0f32; FADE_BUF_SIZE];
        let ndr = ((1.0f32 / 128.0) * FADE_BUF_SIZE as f32) as usize;
        let nr = n - ndr;
        let phi = FPI / (nr * 2) as f32;
        let mut x = 0.0f32;
        for v in &mut rec[ndr..n] {
            *v = -x.sin();
            x += phi;
        }
        rec[n] = 1.0;

        // Pre curve, linear shape: 1 -> 0 over the window, then 0.
        let mut pre = [0.0f32; FADE_BUF_SIZE];
        let nwp = ((1.0f32 / 8.0) * FADE_BUF_SIZE as f32) as usize;
        let phi = 1.0 / nwp as f32;
        let mut x = 0.0f32;
        for v in &mut pre[..nwp] {
            *v = 1.0 - x;
            x += phi;
        }

        Self { rec, pre }
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
        let c = FadeCurves::new();
        assert_eq!(c.pre_value(0.0), 1.0);
        assert_eq!(c.pre_value(1.0), 0.0);
        assert_eq!(c.rec_value(0.0), 0.0);
        assert!((c.rec_value(1.0) + 1.0).abs() < 1e-3);
    }

    #[test]
    fn hermite_passes_through_y1_at_zero() {
        assert_eq!(hermite_f32(0.0, 3.0, 0.25, -1.0, 2.0), 0.25);
        assert_eq!(hermite_f64(0.0, 3.0, 0.25, -1.0, 2.0), 0.25);
    }
}
