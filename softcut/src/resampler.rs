//! Per-sample input resampler feeding a write head. Port of `Resampler.h`
//! (Hermite build; the linear build is compiled out upstream).

use crate::dsp::hermite_f64;

const IN_BUF_MASK: usize = 3;
/// Limits the resampling ratio. Upstream writes out of bounds above rate 64;
/// this port clamps the frame count instead.
pub(crate) const OUT_BUF_FRAMES: usize = 64;

#[derive(Clone)]
pub(crate) struct Resampler {
    rate: f64,
    phi: f64,
    phase: f64,
    in_buf: [f32; IN_BUF_MASK + 1],
    in_idx: usize,
    out_buf: [f32; OUT_BUF_FRAMES],
}

impl Resampler {
    pub(crate) fn new() -> Self {
        Self {
            rate: 1.0,
            phi: 1.0,
            phase: 0.0,
            in_buf: [0.0; IN_BUF_MASK + 1],
            in_idx: 0,
            out_buf: [0.0; OUT_BUF_FRAMES],
        }
    }

    /// Push one input frame; returns how many output frames are in `output()`.
    #[inline]
    pub(crate) fn process_frame(&mut self, x: f32) -> usize {
        self.in_idx = (self.in_idx + 1) & IN_BUF_MASK;
        self.in_buf[self.in_idx] = x;
        if self.rate > 1.0 {
            self.write_up()
        } else {
            self.write_down()
        }
    }

    /// Expects a non-negative rate; the subhead writes backwards for negative rates.
    pub(crate) fn set_rate(&mut self, r: f64) {
        self.rate = r;
        self.phi = 1.0 / r;
    }

    pub(crate) fn set_phase(&mut self, p: f64) {
        self.phase = p;
    }

    #[inline]
    pub(crate) fn output(&self) -> &[f32; OUT_BUF_FRAMES] {
        &self.out_buf
    }

    #[inline]
    fn interpolate(&self, f: f64) -> f32 {
        let b = &self.in_buf;
        let i = self.in_idx;
        hermite_f64(
            f,
            b[(i + 1) & IN_BUF_MASK] as f64,
            b[(i + 2) & IN_BUF_MASK] as f64,
            b[(i + 3) & IN_BUF_MASK] as f64,
            b[i] as f64,
        ) as f32
    }

    // Upstream computes an interpolation for frame 0 and then overwrites it on
    // the first loop pass, so frame k uses offset f0 + (k + 1) * phi. Kept.
    fn write_up(&mut self) -> usize {
        let p = self.phase + self.rate;
        let nf = (p as usize).min(OUT_BUF_FRAMES);
        let mut f = (1.0 - self.phase) * self.phi;
        for k in 0..nf {
            f += self.phi;
            self.out_buf[k] = self.interpolate(f);
        }
        self.phase = p - (p as u32) as f64;
        nf
    }

    fn write_down(&mut self) -> usize {
        let p = self.phase + self.rate;
        let nf = p as usize;
        if nf > 0 {
            let f = (1.0 - self.phase) * self.phi;
            self.out_buf[0] = self.interpolate(f);
            self.phase = p - nf as f64;
        } else {
            self.phase = p;
        }
        nf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn count_frames(rate: f64, inputs: usize) -> usize {
        let mut r = Resampler::new();
        r.set_rate(rate);
        (0..inputs).map(|_| r.process_frame(0.0)).sum()
    }

    #[test]
    fn frame_count_tracks_rate() {
        assert_eq!(count_frames(1.0, 1000), 1000);
        assert_eq!(count_frames(0.5, 1000), 500);
        assert_eq!(count_frames(2.0, 1000), 2000);
        assert_eq!(count_frames(0.0, 1000), 0);
    }

    #[test]
    fn huge_rate_does_not_overrun() {
        assert_eq!(count_frames(1000.0, 3), 3 * OUT_BUF_FRAMES);
    }
}
