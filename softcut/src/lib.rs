//! Rust port of [softcut-lib](https://github.com/monome/softcut-lib), the
//! per-voice looping engine behind monome norns.
//!
//! - [`Voice`]: one crossfaded, resampling read/write head over a buffer the
//!   caller owns and lends per block. No allocation, no threads, no I/O.
//! - [`Engine`]: a multi-voice host with buffers, levels, pan and a feedback
//!   matrix, processing mono input to interleaved output.
//!
//! Neither type is thread-aware. To control a voice from another thread, send
//! [`VoiceCmd`] / [`EngineCmd`] values over a realtime-safe queue and
//! [`apply`](Engine::apply) them on the audio thread before each block. The
//! `rtrb` feature adds the `rt` module, which packages that pattern.
//!
//! By default output tracks the C++ implementation, including two upstream
//! quirks: recorded material is polarity-inverted, and the fade time after
//! reset is 0.1 s. [`Quirks::Fixed`] corrects both.
//!
//! ```
//! use softcut::Voice;
//!
//! let mut buf = vec![0.0f32; 1 << 16]; // length must be a power of two
//! let mut v = Voice::new(48000.0);
//! v.set_loop_start(0.0);
//! v.set_loop_end(1.0);
//! v.set_loop(true);
//! v.set_rec_level(1.0);
//! v.set_rec(true);
//! v.set_play(true);
//! v.cut_to(0.0);
//!
//! let input = [0.25f32; 512];
//! let mut output = [0.0f32; 512];
//! v.process_block(&mut buf, &input, &mut output);
//! assert!(buf.iter().any(|&x| x != 0.0));
//! ```

pub mod buffer;
mod dsp;
mod engine;
mod head;
mod resampler;
#[cfg(feature = "rtrb")]
pub mod rt;
mod svf;
mod voice;

pub use dsp::FadeShape;
pub use engine::{Engine, EngineCmd, EngineConfig, VoiceMix};
pub use voice::{HeadState, Quirks, Voice, VoiceCmd};
