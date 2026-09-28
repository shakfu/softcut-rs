//! Band-limited resampling: a Kaiser-windowed sinc, as a batch function for
//! WAV loads and as a streaming stereo resampler for live input.
//!
//! Both evaluate output frame `j` at input position `j * step` (step = input
//! rate / output rate), with the same taps in the same order, so a stream fed
//! in any chunks produces exactly the batch result.

use std::sync::OnceLock;

/// Kernel half-width, in zero crossings of the sinc.
const ZEROS: usize = 16;
/// Kernel table points per zero crossing.
const OVERSAMPLE: usize = 512;
/// Kaiser window shape: about 80 dB of stopband rejection.
const KAISER_BETA: f64 = 8.0;
/// Cutoff as a fraction of the lower Nyquist, leaving room for the transition band.
const ROLLOFF: f64 = 0.95;

/// Cutoff (normalized to the input rate) and kernel half-width in input frames.
fn cutoff(step: f64) -> (f64, f64) {
    let fc = ROLLOFF * (1.0 / step).min(1.0);
    (fc, ZEROS as f64 / fc)
}

/// Sum `get(k) * kernel(t - k)` over the taps in `lo..=hi`, scaled by `fc`.
#[inline]
fn convolve(t: f64, fc: f64, lo: i64, hi: i64, mut get: impl FnMut(i64) -> f32) -> f32 {
    let table = kernel();
    let mut acc = 0.0f64;
    for k in lo..=hi {
        // Distance from the tap, in zero crossings, as a table position.
        let pos = (t - k as f64).abs() * fc * OVERSAMPLE as f64;
        let j = pos as usize;
        if j + 1 >= table.len() {
            continue;
        }
        let w = table[j] + (table[j + 1] - table[j]) * (pos - j as f64);
        acc += get(k) as f64 * w;
    }
    (acc * fc) as f32
}

/// Fill `out` from `x` read every `step` input samples. Samples outside `x`
/// are 0. Equal rates copy exactly.
pub fn resample(x: &[f32], step: f64, out: &mut [f32]) {
    if step == 1.0 {
        let n = out.len().min(x.len());
        out[..n].copy_from_slice(&x[..n]);
        out[n..].fill(0.0);
        return;
    }
    let (fc, half) = cutoff(step);
    let last = x.len() as i64 - 1;
    for (i, y) in out.iter_mut().enumerate() {
        let t = i as f64 * step;
        let lo = ((t - half).ceil() as i64).max(0);
        let hi = ((t + half).floor() as i64).min(last);
        *y = convolve(t, fc, lo, hi, |k| x[k as usize]);
    }
}

/// Streaming stereo resampler. Allocates only in [`Streamer::new`]; `push` is
/// realtime-safe. Output lags input by the kernel half-width (0.35 ms at
/// 48 kHz).
pub struct Streamer {
    /// Input frames per output frame, from the nominal rates.
    nominal: f64,
    /// `nominal`, adjusted by [`Streamer::set_adjust`].
    step: f64,
    /// Input position of output frame `n_out - n_since`, where the step last
    /// changed. With no adjustment it stays 0, so output `j` is at exactly
    /// `j * step`, as in [`resample`].
    t_base: f64,
    n_since: u64,
    fc: f64,
    half: f64,
    /// Input frames by absolute index, masked. Frames before the first are 0.
    history: Vec<[f32; 2]>,
    mask: usize,
    /// Input frames received.
    n_in: i64,
}

impl Streamer {
    pub fn new(input_rate: f64, output_rate: f64) -> Self {
        let step = input_rate / output_rate;
        let (fc, half) = cutoff(step);
        let len = (2 * half.ceil() as usize + 2).next_power_of_two();
        kernel(); // build the table here, not on the audio thread
        Self {
            nominal: step,
            step,
            t_base: 0.0,
            n_since: 0,
            fc,
            half,
            history: vec![[0.0; 2]; len],
            mask: len - 1,
            n_in: 0,
        }
    }

    /// Scale the ratio by `1 + adjust`, from the next output frame on. Positive
    /// values consume input faster. The filter keeps its nominal cutoff.
    pub fn set_adjust(&mut self, adjust: f64) {
        self.t_base += self.n_since as f64 * self.step;
        self.n_since = 0;
        self.step = self.nominal * (1.0 + adjust);
    }

    /// Take one input frame; call `emit` for each output frame now complete.
    pub fn push(&mut self, frame: [f32; 2], mut emit: impl FnMut([f32; 2])) {
        self.history[self.n_in as usize & self.mask] = frame;
        self.n_in += 1;
        loop {
            let t = self.t_base + self.n_since as f64 * self.step;
            let hi = (t + self.half).floor() as i64;
            if hi > self.n_in - 1 {
                return;
            }
            let lo = ((t - self.half).ceil() as i64).max(0);
            let (h, m) = (&self.history, self.mask);
            let out = [0, 1].map(|c| convolve(t, self.fc, lo, hi, |k| h[k as usize & m][c]));
            emit(out);
            self.n_since += 1;
        }
    }
}

/// Holds a queue's fill level near a target by steering a resampling ratio,
/// so two devices' clocks can drift without the queue running dry or over.
///
/// A proportional controller on the fill level, smoothed over about a second
/// because each consumer callback swings it by a whole block. The adjustment
/// is limited to +/-0.2%, far beyond crystal drift (tens of ppm) and too small
/// a pitch change to hear.
pub struct DriftServo {
    target: f64,
    avg: f64,
}

impl DriftServo {
    const GAIN: f64 = 0.01;
    const SMOOTHING_SECONDS: f64 = 1.0;
    pub const MAX_ADJUST: f64 = 0.002;

    pub fn new(target_frames: f64) -> Self {
        Self {
            target: target_frames,
            avg: target_frames,
        }
    }

    /// Given the fill level in frames, observed `dt` seconds after the last
    /// call, the adjustment for [`Streamer::set_adjust`].
    pub fn update(&mut self, fill: f64, dt: f64) -> f64 {
        self.avg += (fill - self.avg) * (dt / Self::SMOOTHING_SECONDS).min(1.0);
        let error = (self.avg - self.target) / self.target;
        (Self::GAIN * error).clamp(-Self::MAX_ADJUST, Self::MAX_ADJUST)
    }
}

/// `sinc(d) * kaiser(d / ZEROS)` for `d` in [0, ZEROS], `OVERSAMPLE` points
/// per zero crossing. Built once.
fn kernel() -> &'static [f64] {
    static TABLE: OnceLock<Vec<f64>> = OnceLock::new();
    TABLE.get_or_init(|| {
        fn bessel_i0(x: f64) -> f64 {
            let (mut sum, mut term) = (1.0, 1.0);
            for k in 1..50 {
                term *= (x / (2.0 * k as f64)).powi(2);
                sum += term;
            }
            sum
        }
        let norm = bessel_i0(KAISER_BETA);
        (0..=ZEROS * OVERSAMPLE)
            .map(|j| {
                let d = j as f64 / OVERSAMPLE as f64;
                let sinc = if j == 0 {
                    1.0
                } else {
                    (std::f64::consts::PI * d).sin() / (std::f64::consts::PI * d)
                };
                let r = d / ZEROS as f64;
                sinc * bessel_i0(KAISER_BETA * (1.0 - r * r).max(0.0).sqrt()) / norm
            })
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(hz: f64, rate: f64, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| (2.0 * std::f64::consts::PI * hz * i as f64 / rate).sin() as f32)
            .collect()
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
    }

    #[test]
    fn upsampling_reconstructs_a_tone() {
        let x = tone(1000.0, 24000.0, 2400);
        let mut out = vec![0.0; 4800];
        resample(&x, 0.5, &mut out);
        let want = tone(1000.0, 48000.0, 4800);
        // Away from the edges, where the kernel runs past the input.
        let err = (200..4600)
            .map(|i| (out[i] - want[i]).abs())
            .fold(0.0f32, f32::max);
        assert!(err < 1e-3, "max error {err}");
    }

    #[test]
    fn downsampling_passes_band_and_rejects_above_nyquist() {
        let mut out = vec![0.0; 4800];
        // 10 kHz is inside the 48 kHz output band.
        resample(&tone(10000.0, 96000.0, 9600), 2.0, &mut out);
        let pass = rms(&out[200..4600]) / std::f32::consts::FRAC_1_SQRT_2;
        assert!((pass - 1.0).abs() < 0.01, "10 kHz gain {pass}");
        // 30 kHz is above the output Nyquist (24 kHz); linear interpolation
        // would alias it to 18 kHz at nearly full level.
        resample(&tone(30000.0, 96000.0, 9600), 2.0, &mut out);
        let alias_db = 20.0 * (rms(&out[200..4600]) / std::f32::consts::FRAC_1_SQRT_2).log10();
        assert!(alias_db < -70.0, "30 kHz leaks at {alias_db:.1} dB");
    }

    /// Changing the ratio mid-stream keeps the output continuous: no step
    /// larger than the tone's own.
    #[test]
    fn adjusting_the_ratio_is_seamless() {
        let x = tone(1000.0, 48000.0, 20000);
        let mut s = Streamer::new(48000.0, 44100.0);
        let mut out = Vec::new();
        for (i, &v) in x.iter().enumerate() {
            if i % 480 == 0 {
                s.set_adjust(if (i / 480) % 2 == 0 { 0.002 } else { -0.002 });
            }
            s.push([v, v], |f| out.push(f[0]));
        }
        let step = 2.0 * std::f32::consts::PI * 1000.0 / 44100.0;
        let jump = out[100..]
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0, f32::max);
        assert!(jump < 1.01 * step, "jump {jump} vs tone step {step}");
    }

    /// Two minutes of a 48 kHz device running 500 ppm fast into a 44.1 kHz
    /// consumer, modelled by frame counts: with the servo the queue settles
    /// near its target and never runs dry or over; without it, it drifts out.
    #[test]
    fn servo_holds_the_queue_against_drift() {
        const TARGET: f64 = 1024.0;
        let simulate = |servo_on: bool| {
            let (in_rate, out_rate) = (48000.0 * (1.0 + 500e-6), 44100.0);
            let nominal = 48000.0 / 44100.0;
            let (in_block, out_block) = (480.0, 512.0);
            let mut servo = DriftServo::new(TARGET);
            let (mut adjust, mut acc, mut fill) = (0.0, 0.0f64, TARGET);
            let (mut t_in, mut t_out) = (0.0, 0.0);
            let (mut lo, mut hi) = (f64::MAX, f64::MIN);
            while t_out < 120.0 {
                if t_in <= t_out {
                    acc += in_block / (nominal * (1.0 + adjust));
                    fill += acc.floor();
                    acc -= acc.floor();
                    if servo_on {
                        adjust = servo.update(fill, in_block / 48000.0);
                    }
                    t_in += in_block / in_rate;
                } else {
                    fill -= out_block;
                    t_out += out_block / out_rate;
                    if t_out > 30.0 {
                        (lo, hi) = (lo.min(fill), hi.max(fill));
                    }
                }
            }
            (lo, hi)
        };
        let (lo, hi) = simulate(true);
        assert!(lo > 0.0 && hi < 2.0 * TARGET, "servo: fill {lo}..{hi}");
        let (_, hi) = simulate(false);
        assert!(
            hi > 2.0 * TARGET,
            "without the servo the fill stays at {hi}"
        );
    }

    /// Chunked streaming equals the batch result, for up- and downsampling.
    #[test]
    fn stream_matches_batch_in_any_chunks() {
        for (rate_in, rate_out) in [(48000.0, 44100.0), (44100.0, 48000.0), (96000.0, 44100.0)] {
            let step = rate_in / rate_out;
            let l = tone(997.0, rate_in, 5000);
            let r = tone(3001.0, rate_in, 5000);
            let n_out = (5000.0 / step) as usize;
            let (mut want_l, mut want_r) = (vec![0.0; n_out], vec![0.0; n_out]);
            resample(&l, step, &mut want_l);
            resample(&r, step, &mut want_r);

            let mut s = Streamer::new(rate_in, rate_out);
            let mut got = Vec::new();
            // Uneven chunks, as device callbacks deliver them.
            let mut i = 0;
            for chunk in [1usize, 7, 64, 3, 512, 100].iter().cycle() {
                for k in i..(i + chunk).min(5000) {
                    s.push([l[k], r[k]], |f| got.push(f));
                }
                i += chunk;
                if i >= 5000 {
                    break;
                }
            }
            // The stream holds back the last half-kernel of frames.
            assert!(got.len() > n_out - 100, "{} of {n_out}", got.len());
            for (j, f) in got.iter().enumerate() {
                assert_eq!(
                    *f,
                    [want_l[j], want_r[j]],
                    "{rate_in}->{rate_out} frame {j}"
                );
            }
        }
    }
}
