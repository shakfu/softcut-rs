//! WAV loading into a stereo pair of softcut buffers, and saving from one.

use std::path::Path;

use crate::resample::resample;

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
/// with [`resample`](crate::resample::resample).
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

/// Write `channels` (one per file channel) as a 32-bit float WAV, lossless
/// for softcut's f32 buffers. The shortest channel sets the length.
pub fn save(path: &Path, channels: &[&[f32]], rate: u32) -> Result<(), String> {
    let spec = hound::WavSpec {
        channels: channels.len() as u16,
        sample_rate: rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let frames = channels.iter().map(|c| c.len()).min().unwrap_or(0);
    let mut w = hound::WavWriter::create(path, spec).map_err(|e| e.to_string())?;
    for i in 0..frames {
        for c in channels {
            w.write_sample(c[i]).map_err(|e| e.to_string())?;
        }
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
        save(&path, &[&left, &right], 44100).unwrap();
        let l = load(&path, 44100.0, 1024).unwrap();
        std::fs::remove_file(path).ok();
        assert_eq!(l.source_rate, 44100);
        assert_eq!(&l.data[0][..500], &left[..]);
        assert_eq!(&l.data[1][..500], &right[..]);
    }

    #[test]
    fn missing_file_is_an_error() {
        assert!(load(Path::new("/nonexistent.wav"), 48000.0, 1024).is_err());
    }
}
