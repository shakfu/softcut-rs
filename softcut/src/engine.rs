//! Multi-voice host: owns buffers and voices, mixes to interleaved output.
//!
//! Port of softcut-py's `shared/mixer.hpp`: per-voice input gain, a
//! voice-to-voice feedback matrix delayed by one block, and equal-power pan.
//! Allocates only in [`Engine::new`]; [`Engine::process`] and
//! [`Engine::apply`] are realtime-safe.

use crate::voice::{Voice, VoiceCmd};

/// An engine-level change, for hosts that queue changes to the audio thread.
/// Out-of-range indices are ignored.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum EngineCmd {
    Voice(usize, VoiceCmd),
    Level(usize, f32),
    Pan(usize, f32),
    InputGain(usize, f32),
    Feedback {
        src: usize,
        dst: usize,
        amount: f32,
    },
    /// Point a voice at a buffer, by buffer index.
    VoiceBuffer(usize, usize),
    ClearBuffer(usize),
    /// Cut `follow` to `lead`'s position plus `offset` seconds.
    Sync {
        follow: usize,
        lead: usize,
        offset: f32,
    },
}

#[derive(Clone, Copy, Debug)]
pub struct EngineConfig {
    pub sample_rate: f32,
    pub voices: usize,
    pub buffers: usize,
    /// Rounded up to a power of two.
    pub buffer_frames: usize,
    /// Feedback latency, and the largest chunk processed at once.
    pub block_size: usize,
    pub out_channels: usize,
}

impl Default for EngineConfig {
    /// norns layout: 6 voices over 2 buffers of ~5.8 minutes at 48 kHz.
    fn default() -> Self {
        Self {
            sample_rate: 48000.0,
            voices: 6,
            buffers: 2,
            buffer_frames: 1 << 24,
            block_size: 128,
            out_channels: 2,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VoiceMix {
    /// Linear output gain.
    pub level: f32,
    /// -1 (left) .. 1 (right).
    pub pan: f32,
    /// Gain of the engine's external input into this voice.
    pub input_gain: f32,
    /// Index into the engine's buffers.
    pub buffer: usize,
}

impl Default for VoiceMix {
    fn default() -> Self {
        Self {
            level: 1.0,
            pan: 0.0,
            input_gain: 1.0,
            buffer: 0,
        }
    }
}

pub struct Engine {
    voices: Vec<Voice>,
    mix: Vec<VoiceMix>,
    buffers: Vec<Box<[f32]>>,
    /// `fb[src * n + dst]`
    fb: Vec<f32>,
    block_size: usize,
    out_channels: usize,
    voice_in: Vec<f32>,
    prev_out: Vec<f32>,
    cur_out: Vec<f32>,
}

impl Engine {
    /// # Panics
    /// If `voices`, `buffers`, `block_size` or `out_channels` is zero.
    pub fn new(cfg: EngineConfig) -> Self {
        assert!(
            cfg.voices > 0 && cfg.buffers > 0,
            "need at least one voice and one buffer"
        );
        assert!(
            cfg.block_size > 0 && cfg.out_channels > 0,
            "block_size and out_channels must be > 0"
        );
        let frames = cfg.buffer_frames.max(1).next_power_of_two();
        let n = cfg.voices;
        Self {
            voices: (0..n).map(|_| Voice::new(cfg.sample_rate)).collect(),
            mix: vec![VoiceMix::default(); n],
            buffers: (0..cfg.buffers)
                .map(|_| vec![0.0; frames].into_boxed_slice())
                .collect(),
            fb: vec![0.0; n * n],
            block_size: cfg.block_size,
            out_channels: cfg.out_channels,
            voice_in: vec![0.0; cfg.block_size],
            prev_out: vec![0.0; n * cfg.block_size],
            cur_out: vec![0.0; n * cfg.block_size],
        }
    }

    /// Process mono `input` into interleaved `output` of
    /// `input.len() * out_channels` samples.
    ///
    /// # Panics
    /// If `output` has the wrong length.
    pub fn process(&mut self, input: &[f32], output: &mut [f32]) {
        let ch = self.out_channels;
        assert_eq!(
            output.len(),
            input.len() * ch,
            "output must hold input.len() * out_channels samples"
        );
        for (cin, cout) in input
            .chunks(self.block_size)
            .zip(output.chunks_mut(self.block_size * ch))
        {
            self.process_chunk(cin, cout);
        }
    }

    fn process_chunk(&mut self, input: &[f32], out: &mut [f32]) {
        let (n, bs, ch, frames) = (
            self.voices.len(),
            self.block_size,
            self.out_channels,
            input.len(),
        );
        out.fill(0.0);
        let voice_in = &mut self.voice_in[..frames];
        for dst in 0..n {
            let m = self.mix[dst];
            for (vi, &x) in voice_in.iter_mut().zip(input) {
                *vi = x * m.input_gain;
            }
            for src in 0..n {
                let g = self.fb[src * n + dst];
                if g != 0.0 {
                    let po = &self.prev_out[src * bs..src * bs + frames];
                    for (vi, &p) in voice_in.iter_mut().zip(po) {
                        *vi += p * g;
                    }
                }
            }
            let o = &mut self.cur_out[dst * bs..dst * bs + frames];
            self.voices[dst].process_block(&mut self.buffers[m.buffer], voice_in, o);

            let theta = (m.pan * 0.5 + 0.5) * std::f32::consts::FRAC_PI_2;
            let (gl, gr) = (m.level * theta.cos(), m.level * theta.sin());
            for (frame, &y) in out.chunks_exact_mut(ch).zip(o.iter()) {
                frame[0] += y * gl;
                if ch > 1 {
                    frame[1] += y * gr;
                }
            }
        }
        std::mem::swap(&mut self.prev_out, &mut self.cur_out);
    }

    pub fn apply(&mut self, cmd: EngineCmd) {
        let n = self.voices.len();
        match cmd {
            EngineCmd::Voice(i, c) => {
                if let Some(v) = self.voices.get_mut(i) {
                    v.apply(c);
                }
            }
            EngineCmd::Level(i, x) => {
                if let Some(m) = self.mix.get_mut(i) {
                    m.level = x;
                }
            }
            EngineCmd::Pan(i, x) => {
                if let Some(m) = self.mix.get_mut(i) {
                    m.pan = x;
                }
            }
            EngineCmd::InputGain(i, x) => {
                if let Some(m) = self.mix.get_mut(i) {
                    m.input_gain = x;
                }
            }
            EngineCmd::Feedback { src, dst, amount } => {
                if src < n && dst < n {
                    self.fb[src * n + dst] = amount;
                }
            }
            EngineCmd::VoiceBuffer(i, b) => {
                if b < self.buffers.len()
                    && let Some(m) = self.mix.get_mut(i)
                {
                    m.buffer = b;
                }
            }
            EngineCmd::ClearBuffer(b) => {
                if let Some(buf) = self.buffers.get_mut(b) {
                    buf.fill(0.0);
                }
            }
            EngineCmd::Sync {
                follow,
                lead,
                offset,
            } => {
                if follow < n && lead < n {
                    let pos = self.voices[lead].position() + offset;
                    self.voices[follow].cut_to(pos);
                }
            }
        }
    }

    pub fn voices(&self) -> &[Voice] {
        &self.voices
    }

    pub fn voice_mut(&mut self, i: usize) -> &mut Voice {
        &mut self.voices[i]
    }

    pub fn mix(&self, i: usize) -> VoiceMix {
        self.mix[i]
    }

    pub fn feedback(&self, src: usize, dst: usize) -> f32 {
        self.fb[src * self.voices.len() + dst]
    }

    pub fn buffers(&self) -> &[Box<[f32]>] {
        &self.buffers
    }

    /// For editing buffer content in place.
    pub fn buffer_mut(&mut self, i: usize) -> &mut [f32] {
        &mut self.buffers[i]
    }

    /// Swap in `buf` as buffer `i` and return the old one. O(1), no
    /// allocation or deallocation, so it is safe on the audio thread.
    ///
    /// The length may differ from the old buffer's but must be a power of two.
    /// Voices keep their positions; loop points past the new end wrap.
    /// Returns `buf` unchanged as the error if `i` is out of range or the
    /// length is invalid.
    pub fn replace_buffer(&mut self, i: usize, buf: Box<[f32]>) -> Result<Box<[f32]>, Box<[f32]>> {
        match self.buffers.get_mut(i) {
            Some(slot) if buf.len().is_power_of_two() => Ok(std::mem::replace(slot, buf)),
            _ => Err(buf),
        }
    }

    pub fn out_channels(&self) -> usize {
        self.out_channels
    }

    pub fn block_size(&self) -> usize {
        self.block_size
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn small(voices: usize) -> Engine {
        Engine::new(EngineConfig {
            voices,
            buffers: 1,
            buffer_frames: 1 << 16,
            block_size: 64,
            ..Default::default()
        })
    }

    #[test]
    fn buffer_frames_round_up_to_power_of_two() {
        let e = Engine::new(EngineConfig {
            buffer_frames: 1000,
            voices: 1,
            buffers: 1,
            ..Default::default()
        });
        assert_eq!(e.buffers()[0].len(), 1024);
    }

    #[test]
    fn idle_engine_outputs_silence() {
        let mut e = small(2);
        let mut out = vec![1.0; 200 * 2];
        e.process(&[0.5; 200], &mut out);
        assert!(out.iter().all(|&x| x == 0.0));
    }

    #[test]
    fn equal_power_pan() {
        let mut e = small(1);
        e.buffer_mut(0).fill(0.25);
        e.apply(EngineCmd::Voice(0, VoiceCmd::LoopEnd(1.0)));
        e.apply(EngineCmd::Voice(0, VoiceCmd::Loop(true)));
        e.apply(EngineCmd::Voice(0, VoiceCmd::Play(true)));
        let mut out = vec![0.0; 256 * 2];
        for (pan, expect_l, expect_r) in [
            (-1.0, 1.0, 0.0),
            (1.0, 0.0, 1.0),
            (0.0, 0.5f32.sqrt(), 0.5f32.sqrt()),
        ] {
            e.apply(EngineCmd::Pan(0, pan));
            e.process(&[0.0; 256], &mut out);
            let (l, r) = (out[510], out[511]);
            assert!((l - 0.25 * expect_l).abs() < 1e-3, "pan {pan}: l = {l}");
            assert!((r - 0.25 * expect_r).abs() < 1e-3, "pan {pan}: r = {r}");
        }
    }

    #[test]
    fn replace_buffer_swaps_and_validates() {
        let mut e = small(1);
        let old = e
            .replace_buffer(0, vec![0.5; 1 << 10].into_boxed_slice())
            .unwrap();
        assert_eq!(old.len(), 1 << 16);
        assert_eq!(e.buffers()[0].len(), 1 << 10);
        assert_eq!(
            e.replace_buffer(1, vec![0.0; 8].into()).unwrap_err().len(),
            8
        );
        assert_eq!(
            e.replace_buffer(0, vec![0.0; 1000].into())
                .unwrap_err()
                .len(),
            1000
        );

        // A voice keeps running over a shorter buffer.
        e.apply(EngineCmd::Voice(0, VoiceCmd::LoopEnd(1.0)));
        e.apply(EngineCmd::Voice(0, VoiceCmd::Loop(true)));
        e.apply(EngineCmd::Voice(0, VoiceCmd::Play(true)));
        let mut out = vec![0.0; 4096 * 2];
        e.process(&[0.0; 4096], &mut out);
        assert!(out[8000].abs() > 0.1);
    }

    #[test]
    fn out_of_range_commands_are_ignored() {
        let mut e = small(1);
        e.apply(EngineCmd::Level(5, 0.0));
        e.apply(EngineCmd::Feedback {
            src: 0,
            dst: 3,
            amount: 1.0,
        });
        e.apply(EngineCmd::VoiceBuffer(0, 9));
        e.apply(EngineCmd::Voice(7, VoiceCmd::Play(true)));
        assert_eq!(e.mix(0), VoiceMix::default());
    }
}
