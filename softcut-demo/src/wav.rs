//! WAV loading into a stereo pair of softcut buffers, and saving from one.

use std::path::Path;

pub struct Loaded {
    /// Left and right, each exactly the buffer's length and zero-padded past
    /// the file's end. A mono file fills both.
    pub data: [Box<[f32]>; 2],
    /// Duration of the file content in the buffers.
    pub seconds: f32,
    pub truncated: bool,
    pub source_rate: u32,
}

/// Decode any PCM or float WAV into two buffers of `frames` samples at `rate`.
/// Channels past the second are ignored. A file at another rate is resampled
/// with [`resample`].
pub fn load(path: &Path, rate: f32, frames: usize) -> Result<Loaded, String> {
    let mut reader = hound::WavReader::open(path).map_err(|e| e.to_string())?;
    let spec = reader.spec();
    let channels = spec.channels as usize;
    let interleaved: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().collect::<Result<_, _>>(),
        hound::SampleFormat::Int => {
            let scale = 1.0 / (1u64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|s| s.map(|x| x as f32 * scale))
                .collect()
        }
    }
    .map_err(|e| e.to_string())?;
    let total = interleaved.len() / channels;

    let step = spec.sample_rate as f64 / rate as f64;
    let wanted = (total as f64 / step).floor() as usize;
    let n = wanted.min(frames);
    // The channels are independent, so resample them in parallel.
    let side = |col: usize| {
        let col = col.min(channels - 1);
        let channel: Vec<f32> = interleaved
            .iter()
            .skip(col)
            .step_by(channels)
            .copied()
            .collect();
        let mut out = vec![0.0f32; frames].into_boxed_slice();
        resample(&channel, step, &mut out[..n]);
        out
    };
    let data = std::thread::scope(|s| {
        let right = s.spawn(|| side(1));
        [side(0), right.join().expect("resampling thread panicked")]
    });
    Ok(Loaded {
        data,
        seconds: n as f32 / rate,
        truncated: wanted > frames,
        source_rate: spec.sample_rate,
    })
}

/// Kernel half-width, in zero crossings of the sinc.
const ZEROS: usize = 16;
/// Kernel table points per zero crossing.
const OVERSAMPLE: usize = 512;
/// Kaiser window shape: about 80 dB of stopband rejection.
const KAISER_BETA: f64 = 8.0;
/// Cutoff as a fraction of the lower Nyquist, leaving room for the transition band.
const ROLLOFF: f64 = 0.95;

/// Fill `out` from `x` read every `step` input samples (input rate / output
/// rate), band-limited with a Kaiser-windowed sinc. Samples outside `x` are 0.
/// Equal rates copy exactly.
pub fn resample(x: &[f32], step: f64, out: &mut [f32]) {
    if step == 1.0 {
        let n = out.len().min(x.len());
        out[..n].copy_from_slice(&x[..n]);
        out[n..].fill(0.0);
        return;
    }
    // Normalized to the input rate: the lower Nyquist, less the rolloff.
    let fc = ROLLOFF * (1.0 / step).min(1.0);
    let half = ZEROS as f64 / fc;
    let table = kernel_table();
    let last = x.len() as isize - 1;
    for (i, y) in out.iter_mut().enumerate() {
        let t = i as f64 * step;
        let lo = ((t - half).ceil() as isize).max(0);
        let hi = ((t + half).floor() as isize).min(last);
        let mut acc = 0.0f64;
        for k in lo..=hi {
            // Distance from the tap, in zero crossings, as a table position.
            let pos = (t - k as f64).abs() * fc * OVERSAMPLE as f64;
            let j = pos as usize;
            if j + 1 >= table.len() {
                continue;
            }
            let w = table[j] + (table[j + 1] - table[j]) * (pos - j as f64);
            acc += x[k as usize] as f64 * w;
        }
        *y = (acc * fc) as f32;
    }
}

/// `sinc(d) * kaiser(d / ZEROS)` for `d` in [0, ZEROS], `OVERSAMPLE` points
/// per zero crossing.
fn kernel_table() -> Vec<f64> {
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
}

/// Write `left` and `right` as a stereo 32-bit float WAV, lossless for
/// softcut's f32 buffers. The shorter channel sets the length.
pub fn save(path: &Path, left: &[f32], right: &[f32], rate: u32) -> Result<(), String> {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut w = hound::WavWriter::create(path, spec).map_err(|e| e.to_string())?;
    for (&l, &r) in left.iter().zip(right) {
        w.write_sample(l).map_err(|e| e.to_string())?;
        w.write_sample(r).map_err(|e| e.to_string())?;
    }
    w.finalize().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("softcut-demo-{}-{name}.wav", std::process::id()))
    }

    fn write(name: &str, spec: hound::WavSpec, samples: &[f32]) -> std::path::PathBuf {
        let path = temp(name);
        let mut w = hound::WavWriter::create(&path, spec).unwrap();
        for &s in samples {
            match spec.sample_format {
                hound::SampleFormat::Float => w.write_sample(s).unwrap(),
                hound::SampleFormat::Int => w.write_sample((s * 32767.0) as i16).unwrap(),
            }
        }
        w.finalize().unwrap();
        path
    }

    #[test]
    fn stereo_int16_keeps_channels_and_upsamples() {
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 24000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        // Left 0.5, right -0.25 for 2400 frames (0.1 s).
        let samples: Vec<f32> = (0..2400).flat_map(|_| [0.5, -0.25]).collect();
        let path = write("int16", spec, &samples);
        let l = load(&path, 48000.0, 1 << 14).unwrap();
        std::fs::remove_file(path).ok();
        assert_eq!(l.data[0].len(), 1 << 14);
        assert!((l.seconds - 0.1).abs() < 1e-3, "{}", l.seconds);
        assert!(!l.truncated);
        assert!((l.data[0][100] - 0.5).abs() < 1e-3, "{}", l.data[0][100]);
        assert!((l.data[1][100] + 0.25).abs() < 1e-3, "{}", l.data[1][100]);
        assert_eq!((l.data[0][4800], l.data[1][4800]), (0.0, 0.0));
    }

    #[test]
    fn float_mono_fills_both_sides_and_truncates() {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 48000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let samples: Vec<f32> = (0..3000).map(|i| i as f32 / 3000.0).collect();
        let path = write("f32", spec, &samples);
        let l = load(&path, 48000.0, 1024).unwrap();
        std::fs::remove_file(path).ok();
        assert!(l.truncated);
        assert_eq!(l.data[0], l.data[1]);
        assert_eq!(l.data[0][1023], 1023.0 / 3000.0);
    }

    #[test]
    fn save_then_load_round_trips() {
        let left: Vec<f32> = (0..500).map(|i| (i as f32 * 0.01).sin()).collect();
        let right: Vec<f32> = left.iter().map(|x| -x * 0.5).collect();
        let path = temp("roundtrip");
        save(&path, &left, &right, 44100).unwrap();
        let l = load(&path, 44100.0, 1024).unwrap();
        std::fs::remove_file(path).ok();
        assert_eq!(l.source_rate, 44100);
        assert_eq!(&l.data[0][..500], &left[..]);
        assert_eq!(&l.data[1][..500], &right[..]);
    }

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

    #[test]
    fn missing_file_is_an_error() {
        assert!(load(Path::new("/nonexistent.wav"), 48000.0, 1024).is_err());
    }
}
