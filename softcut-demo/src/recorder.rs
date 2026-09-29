//! Recording the output mix to a WAV file.
//!
//! The audio thread copies each rendered stereo block into a ring (see
//! `AudioState::record`); a writer thread here drains it into the file, so
//! the audio thread does no file I/O. The ring outlives recordings: `stop`
//! hands its consumer back for the next one.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::thread::JoinHandle;
use std::time::Duration;

use rtrb::Consumer;

/// How the writer thread ended: the ring back, and frames written or an error.
type Finished = (Consumer<f32>, Result<u64, String>);

pub struct Recorder {
    pub path: PathBuf,
    /// Frames written so far.
    frames: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    /// Taken by `stop`; joined on drop otherwise, so the file is finalized
    /// even if the app exits mid-recording.
    thread: Option<JoinHandle<Finished>>,
}

impl Recorder {
    /// Create a stereo 32-bit float WAV at `path` and start draining `rx`
    /// into it. Anything already in `rx` is discarded first, so a file never
    /// starts with the tail of the previous one. On failure `rx` comes back.
    pub fn start(
        mut rx: Consumer<f32>,
        path: &Path,
        rate: u32,
    ) -> Result<Self, (Consumer<f32>, String)> {
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: rate,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut writer = match hound::WavWriter::create(path, spec) {
            Ok(w) => w,
            Err(e) => return Err((rx, e.to_string())),
        };
        if let Ok(stale) = rx.read_chunk(rx.slots()) {
            stale.commit_all();
        }
        let (frames, stop) = (
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicBool::new(false)),
        );
        let thread = {
            let (frames, stop) = (frames.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut error = None;
                let mut samples = 0u64;
                loop {
                    // Read the flag before draining, so the last drain comes
                    // after the caller stopped the audio thread's copying.
                    let stopping = stop.load(Relaxed);
                    if let Ok(chunk) = rx.read_chunk(rx.slots()) {
                        let (a, b) = chunk.as_slices();
                        for &x in a.iter().chain(b) {
                            if error.is_none()
                                && let Err(e) = writer.write_sample(x)
                            {
                                error = Some(e.to_string());
                            }
                        }
                        samples += (a.len() + b.len()) as u64;
                        frames.store(samples / 2, Relaxed);
                        chunk.commit_all();
                    }
                    if stopping {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                // Finalize even after a write error, to leave a valid header.
                let finalized = writer.finalize().map_err(|e| e.to_string());
                let result = match error {
                    Some(e) => Err(e),
                    None => finalized.map(|()| samples / 2),
                };
                (rx, result)
            })
        };
        Ok(Self {
            path: path.to_path_buf(),
            frames,
            stop,
            thread: Some(thread),
        })
    }

    /// Frames written so far.
    pub fn frames(&self) -> u64 {
        self.frames.load(Relaxed)
    }

    /// Finish the file after draining what remains. Call it after the audio
    /// thread has stopped copying.
    pub fn stop(mut self) -> Finished {
        self.stop.store(true, Relaxed);
        let thread = self.thread.take().expect("stopped once");
        thread.join().expect("recorder thread panicked")
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take() {
            self.stop.store(true, Relaxed);
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rtrb::RingBuffer;

    #[test]
    fn writes_what_was_queued_and_hands_the_ring_back() {
        let path = std::env::temp_dir().join(format!("softcut-rec-{}.wav", std::process::id()));
        let (mut tx, rx) = RingBuffer::<f32>::new(1024);
        // Stale samples from before the recording are discarded.
        tx.push_entire_slice(&[9.0, 9.0]).unwrap();
        let rec = Recorder::start(rx, &path, 44100).map_err(|e| e.1).unwrap();
        let block: Vec<f32> = (0..400).map(|i| i as f32 / 400.0).collect();
        for _ in 0..10 {
            while tx.push_entire_slice(&block).is_err() {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        let (rx, result) = rec.stop();
        assert_eq!(result, Ok(2000));
        assert_eq!(rx.slots(), 0);

        let mut reader = hound::WavReader::open(&path).unwrap();
        assert_eq!(reader.spec().channels, 2);
        let got: Vec<f32> = reader.samples::<f32>().map(Result::unwrap).collect();
        std::fs::remove_file(&path).ok();
        assert_eq!(got.len(), 4000);
        assert!(got.chunks(400).all(|c| c == block.as_slice()));
    }

    #[test]
    fn unwritable_path_returns_the_ring() {
        let (_tx, rx) = RingBuffer::<f32>::new(16);
        let err = Recorder::start(rx, Path::new("/nonexistent/dir/x.wav"), 44100)
            .err()
            .unwrap();
        assert!(!err.1.is_empty());
    }
}
