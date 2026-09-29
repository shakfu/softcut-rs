//! Multi-voice host: owns buffers and voices, mixes to interleaved output.
//!
//! Port of softcut-py's `shared/mixer.hpp`: per-voice input gain, a
//! voice-to-voice feedback matrix delayed by one block, and equal-power pan.
//! Adds multichannel input with a channel-to-voice level matrix, as on norns.
//! Allocates only in [`Engine::new`]; [`Engine::process`] and
//! [`Engine::apply`] are realtime-safe.

use crate::buffer;
use crate::voice::{Quirks, Voice, VoiceCmd};

/// An engine-level change, for hosts that queue changes to the audio thread.
/// Out-of-range indices are ignored.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum EngineCmd {
    Voice(usize, VoiceCmd),
    Level(usize, f32),
    Pan(usize, f32),
    /// Gain applied to all of a voice's input, after the channel levels.
    InputGain(usize, f32),
    /// Level of input `channel` into `voice` (norns `level_input_cut`).
    InputLevel {
        channel: usize,
        voice: usize,
        amount: f32,
    },
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
    /// Scale a region by `preserve` (0 silences it), fading over `fade`
    /// seconds at each edge. Times in seconds; a negative `len` runs to the
    /// end. See [`buffer::clear`].
    ClearRegion {
        buffer: usize,
        start: f32,
        len: f32,
        fade: f32,
        preserve: f32,
    },
    /// Copy a region between or within buffers, blended as in
    /// [`buffer::copy`]. A reversed copy between partly overlapping regions of
    /// one buffer is ignored; see [`buffer::copy_within`].
    CopyRegion {
        src: usize,
        dst: usize,
        src_start: f32,
        dst_start: f32,
        len: f32,
        fade: f32,
        preserve: f32,
        reverse: bool,
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
    /// Interleaved input channels. Voice `v` initially listens to channel
    /// `v % in_channels` at level 1.
    pub in_channels: usize,
    pub out_channels: usize,
    pub quirks: Quirks,
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
            in_channels: 1,
            out_channels: 2,
            quirks: Quirks::Upstream,
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
    /// `in_level[channel * n + voice]`
    in_level: Vec<f32>,
    sample_rate: f32,
    block_size: usize,
    in_channels: usize,
    out_channels: usize,
    voice_in: Vec<f32>,
    prev_out: Vec<f32>,
    cur_out: Vec<f32>,
}

impl Engine {
    /// # Panics
    /// If `voices`, `buffers`, `block_size`, `in_channels` or `out_channels`
    /// is zero.
    pub fn new(cfg: EngineConfig) -> Self {
        assert!(
            cfg.voices > 0 && cfg.buffers > 0,
            "need at least one voice and one buffer"
        );
        assert!(
            cfg.block_size > 0 && cfg.in_channels > 0 && cfg.out_channels > 0,
            "block_size, in_channels and out_channels must be > 0"
        );
        let frames = cfg.buffer_frames.max(1).next_power_of_two();
        let (n, ic) = (cfg.voices, cfg.in_channels);
        let in_level = (0..ic * n)
            .map(|k| if (k % n) % ic == k / n { 1.0 } else { 0.0 })
            .collect();
        Self {
            voices: (0..n)
                .map(|_| Voice::with_quirks(cfg.sample_rate, cfg.quirks))
                .collect(),
            mix: vec![VoiceMix::default(); n],
            buffers: (0..cfg.buffers)
                .map(|_| vec![0.0; frames].into_boxed_slice())
                .collect(),
            fb: vec![0.0; n * n],
            in_level,
            sample_rate: cfg.sample_rate,
            block_size: cfg.block_size,
            in_channels: ic,
            out_channels: cfg.out_channels,
            voice_in: vec![0.0; cfg.block_size],
            prev_out: vec![0.0; n * cfg.block_size],
            cur_out: vec![0.0; n * cfg.block_size],
        }
    }

    /// Process interleaved `input` of `in_channels` into interleaved `output`
    /// of `out_channels`, frame for frame.
    ///
    /// # Panics
    /// If `input` is not whole frames, or `output` has a different frame count.
    pub fn process(&mut self, input: &[f32], output: &mut [f32]) {
        self.process_with(input, output, |_, _| {});
    }

    /// [`process`](Self::process), running `insert(voice, block)` on each
    /// voice's mono output before it is panned into the mix: a per-voice
    /// effects insert. The voice-to-voice feedback carries the processed
    /// block. `insert` must be realtime-safe.
    pub fn process_with(
        &mut self,
        input: &[f32],
        output: &mut [f32],
        mut insert: impl FnMut(usize, &mut [f32]),
    ) {
        let (ic, oc) = (self.in_channels, self.out_channels);
        assert_eq!(
            input.len() % ic,
            0,
            "input must be whole frames of in_channels"
        );
        assert_eq!(
            output.len(),
            input.len() / ic * oc,
            "output must hold as many frames as input"
        );
        for (cin, cout) in input
            .chunks(self.block_size * ic)
            .zip(output.chunks_mut(self.block_size * oc))
        {
            self.process_chunk(cin, cout, &mut insert);
        }
    }

    fn process_chunk(
        &mut self,
        input: &[f32],
        out: &mut [f32],
        insert: &mut impl FnMut(usize, &mut [f32]),
    ) {
        let (n, bs, ic, ch) = (
            self.voices.len(),
            self.block_size,
            self.in_channels,
            self.out_channels,
        );
        let frames = input.len() / ic;
        out.fill(0.0);
        let voice_in = &mut self.voice_in[..frames];
        for dst in 0..n {
            let m = self.mix[dst];
            voice_in.fill(0.0);
            for c in 0..ic {
                let g = self.in_level[c * n + dst];
                if g != 0.0 {
                    for (vi, frame) in voice_in.iter_mut().zip(input.chunks_exact(ic)) {
                        *vi += frame[c] * g;
                    }
                }
            }
            for vi in voice_in.iter_mut() {
                *vi *= m.input_gain;
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
            insert(dst, o);

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
            EngineCmd::InputLevel {
                channel,
                voice,
                amount,
            } => {
                if channel < self.in_channels && voice < n {
                    self.in_level[channel * n + voice] = amount;
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
            EngineCmd::ClearRegion {
                buffer,
                start,
                len,
                fade,
                preserve,
            } => {
                let (start, fade) = (self.frames(start), self.frames(fade));
                let len = if len < 0.0 {
                    usize::MAX
                } else {
                    self.frames(len)
                };
                if let Some(buf) = self.buffers.get_mut(buffer) {
                    buffer::clear(buf, start, len, preserve, fade);
                }
            }
            EngineCmd::CopyRegion {
                src,
                dst,
                src_start,
                dst_start,
                len,
                fade,
                preserve,
                reverse,
            } => {
                let (ss, ds, fade) = (
                    self.frames(src_start),
                    self.frames(dst_start),
                    self.frames(fade),
                );
                let len = if len < 0.0 {
                    usize::MAX
                } else {
                    self.frames(len)
                };
                if src == dst {
                    if let Some(buf) = self.buffers.get_mut(src) {
                        buffer::copy_within(buf, ss, ds, len, preserve, fade, reverse);
                    }
                } else if let Ok([s, d]) = self.buffers.get_disjoint_mut([src, dst]) {
                    buffer::copy(s, d, ss, ds, len, preserve, fade, reverse);
                }
            }
        }
    }

    /// Seconds to frames, rounded; negative times clamp to 0.
    fn frames(&self, sec: f32) -> usize {
        (sec * self.sample_rate).round().max(0.0) as usize
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

    pub fn input_level(&self, channel: usize, voice: usize) -> f32 {
        self.in_level[channel * self.voices.len() + voice]
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

    pub fn in_channels(&self) -> usize {
        self.in_channels
    }

    pub fn sample_rate(&self) -> f32 {
        self.sample_rate
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

    fn record_all(e: &mut Engine) {
        for v in 0..e.voices().len() {
            for c in [
                VoiceCmd::LoopEnd(0.5),
                VoiceCmd::Loop(true),
                VoiceCmd::RecLevel(1.0),
                VoiceCmd::Rec(true),
            ] {
                e.apply(EngineCmd::Voice(v, c));
            }
        }
    }

    #[test]
    fn stereo_input_routes_channel_per_voice() {
        let mut e = Engine::new(EngineConfig {
            voices: 2,
            buffers: 2,
            buffer_frames: 1 << 15,
            block_size: 64,
            in_channels: 2,
            quirks: Quirks::Fixed,
            ..Default::default()
        });
        e.apply(EngineCmd::VoiceBuffer(1, 1));
        record_all(&mut e);
        // L = 0.1, R = 0.2, interleaved; voice 0 hears L, voice 1 hears R.
        let input: Vec<f32> = (0..4800).flat_map(|_| [0.1, 0.2]).collect();
        let mut out = vec![0.0; 4800 * 2];
        e.process(&input, &mut out);
        // Soft clip gain 1.2 below the knee.
        assert!(
            (e.buffers()[0][3000] - 0.12).abs() < 1e-3,
            "{}",
            e.buffers()[0][3000]
        );
        assert!(
            (e.buffers()[1][3000] - 0.24).abs() < 1e-3,
            "{}",
            e.buffers()[1][3000]
        );

        // Route both channels into voice 0 at half level.
        e.apply(EngineCmd::InputLevel {
            channel: 0,
            voice: 0,
            amount: 0.5,
        });
        e.apply(EngineCmd::InputLevel {
            channel: 1,
            voice: 0,
            amount: 0.5,
        });
        assert_eq!((e.input_level(0, 0), e.input_level(1, 0)), (0.5, 0.5));
        assert_eq!((e.input_level(0, 1), e.input_level(1, 1)), (0.0, 1.0));
    }

    #[test]
    fn engine_quirks_reach_every_voice() {
        let e = Engine::new(EngineConfig {
            voices: 3,
            buffers: 1,
            buffer_frames: 1 << 10,
            quirks: Quirks::Fixed,
            ..Default::default()
        });
        assert!(e.voices().iter().all(|v| v.quirks() == Quirks::Fixed));
    }

    #[test]
    fn region_commands_use_seconds() {
        let mut e = Engine::new(EngineConfig {
            sample_rate: 1000.0,
            voices: 1,
            buffers: 2,
            buffer_frames: 1 << 12,
            ..Default::default()
        });
        for (i, x) in e.buffer_mut(0).iter_mut().enumerate() {
            *x = i as f32;
        }
        e.apply(EngineCmd::ClearRegion {
            buffer: 0,
            start: 1.0,
            len: 0.5,
            fade: 0.0,
            preserve: 0.0,
        });
        let b = &e.buffers()[0];
        assert_eq!(
            (b[999], b[1000], b[1499], b[1500]),
            (999.0, 0.0, 0.0, 1500.0)
        );

        // Reverse 0.1 s from buffer 0 at 2 s into buffer 1 at 0.5 s.
        e.apply(EngineCmd::CopyRegion {
            src: 0,
            dst: 1,
            src_start: 2.0,
            dst_start: 0.5,
            len: 0.1,
            fade: 0.0,
            preserve: 0.0,
            reverse: true,
        });
        let b = &e.buffers()[1];
        assert_eq!((b[499], b[500], b[599], b[600]), (0.0, 2099.0, 2000.0, 0.0));

        // Reverse in place within one buffer.
        e.apply(EngineCmd::CopyRegion {
            src: 0,
            dst: 0,
            src_start: 3.0,
            dst_start: 3.0,
            len: 0.01,
            fade: 0.0,
            preserve: 0.0,
            reverse: true,
        });
        let b = &e.buffers()[0];
        assert_eq!((b[3000], b[3009]), (3009.0, 3000.0));

        // A negative length clears to the end.
        e.apply(EngineCmd::ClearRegion {
            buffer: 0,
            start: 4.0,
            len: -1.0,
            fade: 0.0,
            preserve: 0.0,
        });
        assert!(e.buffers()[0][4000..].iter().all(|&x| x == 0.0));
    }

    /// An insert processes each voice before the mix, and feedback carries
    /// its result.
    #[test]
    fn insert_runs_per_voice_before_mix_and_feedback() {
        let mut e = small(2);
        e.buffer_mut(0).fill(0.25);
        for v in 0..2 {
            e.apply(EngineCmd::Voice(v, VoiceCmd::LoopEnd(1.0)));
            e.apply(EngineCmd::Voice(v, VoiceCmd::Loop(true)));
            e.apply(EngineCmd::Pan(v, -1.0));
        }
        e.apply(EngineCmd::Voice(0, VoiceCmd::Play(true)));
        let mut out = vec![0.0; 256 * 2];
        let mut seen = [0usize; 2];
        // Silence voice 0 in its insert: nothing reaches the mix.
        e.process_with(&[0.0; 256], &mut out, |v, block| {
            seen[v] += block.len();
            if v == 0 {
                block.fill(0.0);
            }
        });
        assert_eq!(seen, [256, 256]);
        assert!(out.iter().all(|&x| x == 0.0));
        // Voice 0 feeds voice 1, which records it. With voice 0 silenced in
        // the insert, voice 1 records nothing.
        e.apply(EngineCmd::Feedback {
            src: 0,
            dst: 1,
            amount: 1.0,
        });
        e.apply(EngineCmd::InputGain(1, 0.0));
        e.apply(EngineCmd::VoiceBuffer(1, 0));
        for c in [
            VoiceCmd::RecLevel(1.0),
            VoiceCmd::Rec(true),
            VoiceCmd::LoopStart(0.8),
            VoiceCmd::LoopEnd(1.2),
            VoiceCmd::CutTo(0.8),
        ] {
            e.apply(EngineCmd::Voice(1, c));
        }
        for _ in 0..40 {
            e.process_with(&[0.0; 256], &mut out, |v, block| {
                if v == 0 {
                    block.fill(0.0);
                }
            });
        }
        // The buffer held 0.25 there. Past the 0.1 s crossfade (4800 frames),
        // voice 1 has overwritten it with what the insert passed: silence.
        let at = (0.8 * 48000.0) as usize;
        let region = &e.buffers()[0][at + 5500..at + 9500];
        assert!(
            region.iter().all(|&x| x == 0.0),
            "feedback bypassed the insert"
        );
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
