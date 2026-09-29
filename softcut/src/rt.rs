//! Cross-thread control of an [`Engine`] (feature `rtrb`).
//!
//! [`split`] divides an engine into a [`Handle`] for the control thread and a
//! [`Processor`] for the audio thread. Commands and buffer transfers (load,
//! write, snapshot) travel over one wait-free SPSC ring, so they apply in the
//! order they were sent, at the start of the next [`Processor::process`] call.
//! Head positions and rec/play flags come back through atomics, published once
//! per call. Every buffer sent comes back over a second ring as a
//! [`Returned`], so the audio thread never frees memory.
//!
//! The ring has one producer. Hosts with several control sources must
//! serialize them onto the one `Handle`.
//!
//! # Reading settings back
//!
//! The [`Voice`](crate::Voice) getters are unavailable on the control thread,
//! since the voices live in the [`Processor`]. Every setting reaches them
//! through the one `Handle`, though, so the control thread can keep a shadow
//! voice: apply each [`VoiceCmd`](crate::VoiceCmd) to a local `Voice` before
//! sending it, and read settings from the shadow's getters. The shadow
//! processes no audio; construct it with the same sample rate and
//! [`Quirks`](crate::Quirks), and `Reset` restores its defaults exactly as on
//! the audio thread.
//!
//! The shadow's playback state goes stale: rec-once ends, and heads move, only
//! on the audio thread. Read those from [`Handle::rec`], [`Handle::play`] and
//! [`Handle::position`].
//!
//! ```
//! use softcut::rt;
//! use softcut::{Engine, EngineCmd, EngineConfig, Voice, VoiceCmd};
//!
//! let cfg = EngineConfig { voices: 1, buffers: 1, buffer_frames: 1 << 14, ..Default::default() };
//! let (mut handle, _processor) = rt::split(Engine::new(cfg), 64);
//! let mut shadow = Voice::with_quirks(cfg.sample_rate, cfg.quirks);
//!
//! let mut set = |cmd: VoiceCmd| {
//!     shadow.apply(cmd);
//!     handle.send(EngineCmd::Voice(0, cmd))
//! };
//! set(VoiceCmd::Rate(-0.5)).unwrap();
//! set(VoiceCmd::Reset).unwrap();
//! assert_eq!(shadow.rate(), 1.0);
//! assert_eq!(shadow.fade_time(), 0.1); // the upstream quirk, as on the audio thread
//! ```

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};

use rtrb::{Consumer, Producer, RingBuffer};

use crate::{Engine, EngineCmd, HeadState, buffer};

enum Msg {
    Cmd(EngineCmd),
    Load(usize, Box<[f32]>),
    Write {
        buffer: usize,
        start: f32,
        data: Box<[f32]>,
        preserve: f32,
        mix: f32,
        fade: f32,
    },
    Snapshot(usize, Box<[f32]>),
}

impl Msg {
    fn into_data(self) -> Option<Box<[f32]>> {
        match self {
            Msg::Cmd(_) => None,
            Msg::Load(_, d) | Msg::Write { data: d, .. } | Msg::Snapshot(_, d) => Some(d),
        }
    }
}

/// A buffer coming back from the audio thread.
#[derive(Debug, PartialEq)]
pub enum Returned {
    /// What `buffer` held before a [`Handle::load`] replaced it.
    Replaced { buffer: usize, data: Box<[f32]> },
    /// The data passed to [`Handle::write`], after it was written.
    Written { buffer: usize, data: Box<[f32]> },
    /// The buffer passed to [`Handle::snapshot`], holding a copy of the first
    /// `min(data.len(), buffer length)` frames of `buffer`.
    Snapshot { buffer: usize, data: Box<[f32]> },
}

impl Returned {
    pub fn buffer(&self) -> usize {
        match self {
            Returned::Replaced { buffer, .. }
            | Returned::Written { buffer, .. }
            | Returned::Snapshot { buffer, .. } => *buffer,
        }
    }

    pub fn into_data(self) -> Box<[f32]> {
        match self {
            Returned::Replaced { data, .. }
            | Returned::Written { data, .. }
            | Returned::Snapshot { data, .. } => data,
        }
    }
}

struct Shared {
    /// Seconds, as f32 bits.
    position: Box<[AtomicU32]>,
    rec: Box<[AtomicBool]>,
    play: Box<[AtomicBool]>,
    /// Per voice and head: position and fade as f32 bits, and the active flag.
    heads: Box<[[(AtomicU32, AtomicU32, AtomicBool); 2]]>,
}

/// Returns the control half and the audio half of `engine`. `capacity` is the
/// number of messages that can be queued between two `process` calls, and
/// the number of buffers that can wait for [`Handle::returned`].
pub fn split(engine: Engine, capacity: usize) -> (Handle, Processor) {
    let n = engine.voices().len();
    let buffers = engine.buffers().len();
    let shared = Arc::new(Shared {
        position: (0..n).map(|_| AtomicU32::new(0)).collect(),
        rec: (0..n).map(|_| AtomicBool::new(false)).collect(),
        play: (0..n).map(|_| AtomicBool::new(false)).collect(),
        heads: (0..n)
            .map(|_| {
                std::array::from_fn(|_| {
                    (AtomicU32::new(0), AtomicU32::new(0), AtomicBool::new(false))
                })
            })
            .collect(),
    });
    let (tx, rx) = RingBuffer::new(capacity);
    let (spent_tx, spent_rx) = RingBuffer::new(capacity);
    let mut processor = Processor {
        engine,
        rx,
        spent: spent_tx,
        shared: shared.clone(),
    };
    processor.publish();
    let handle = Handle {
        tx,
        returned: spent_rx,
        shared,
        buffers,
        dropped: 0,
    };
    (handle, processor)
}

/// The ring was full; the command is returned unapplied.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Full(pub EngineCmd);

impl fmt::Display for Full {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "softcut command ring full; dropped {:?}", self.0)
    }
}

impl std::error::Error for Full {}

/// Why [`Handle::load`], [`Handle::write`] or [`Handle::snapshot`] refused a buffer.
#[derive(Debug, PartialEq)]
pub enum BufferError {
    /// No buffer has this index.
    NoSuchBuffer(usize),
    /// The length, which is not a power of two ([`Handle::load`] only).
    NotPowerOfTwo(usize),
    /// The ring was full; the data is returned for a later retry.
    Full(Box<[f32]>),
}

impl fmt::Display for BufferError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BufferError::NoSuchBuffer(i) => write!(f, "no softcut buffer {i}"),
            BufferError::NotPowerOfTwo(n) => write!(f, "buffer length {n} is not a power of two"),
            BufferError::Full(_) => write!(f, "softcut command ring full"),
        }
    }
}

impl std::error::Error for BufferError {}

/// Control-thread half: sends commands and buffers, reads published state.
/// Never blocks.
pub struct Handle {
    tx: Producer<Msg>,
    returned: Consumer<Returned>,
    shared: Arc<Shared>,
    buffers: usize,
    dropped: u64,
}

impl Handle {
    /// Queue a command for the next `process` call.
    ///
    /// A full ring means the audio thread is not keeping up or has stopped.
    /// The command is returned in the error and counted in [`dropped`](Self::dropped).
    pub fn send(&mut self, cmd: EngineCmd) -> Result<(), Full> {
        self.tx
            .push(Msg::Cmd(cmd))
            .map_err(|rtrb::PushError::Full(_)| {
                self.dropped += 1;
                Full(cmd)
            })
    }

    /// Queue `data` to replace buffer `buffer`, ordered with [`send`](Self::send).
    ///
    /// The length may differ from the current buffer's but must be a power of
    /// two. The old buffer comes back through [`returned`](Self::returned).
    pub fn load(&mut self, buffer: usize, data: Box<[f32]>) -> Result<(), BufferError> {
        if buffer >= self.buffers {
            return Err(BufferError::NoSuchBuffer(buffer));
        }
        if !data.len().is_power_of_two() {
            return Err(BufferError::NotPowerOfTwo(data.len()));
        }
        self.push(Msg::Load(buffer, data))
    }

    /// Queue a blended write of `data` into `buffer` at `start` seconds, as in
    /// [`buffer::write`] with `fade` in seconds. With `preserve = 0, mix = 1`,
    /// an overwrite: norns `buffer_read`, with the file decoded by the caller.
    /// `data` comes back as [`Returned::Written`].
    pub fn write(
        &mut self,
        buffer: usize,
        start: f32,
        data: Box<[f32]>,
        preserve: f32,
        mix: f32,
        fade: f32,
    ) -> Result<(), BufferError> {
        if buffer >= self.buffers {
            return Err(BufferError::NoSuchBuffer(buffer));
        }
        self.push(Msg::Write {
            buffer,
            start,
            data,
            preserve,
            mix,
            fade,
        })
    }

    /// Queue a copy of `buffer` into `dest`, taken between two blocks, so it
    /// is consistent even while voices record. `dest` comes back as
    /// [`Returned::Snapshot`]. The copy costs one memcpy on the audio thread.
    pub fn snapshot(&mut self, buffer: usize, dest: Box<[f32]>) -> Result<(), BufferError> {
        if buffer >= self.buffers {
            return Err(BufferError::NoSuchBuffer(buffer));
        }
        self.push(Msg::Snapshot(buffer, dest))
    }

    fn push(&mut self, msg: Msg) -> Result<(), BufferError> {
        self.tx.push(msg).map_err(|rtrb::PushError::Full(msg)| {
            self.dropped += 1;
            BufferError::Full(
                msg.into_data()
                    .expect("only buffer messages are pushed here"),
            )
        })
    }

    /// Next buffer back from the audio thread, oldest first.
    ///
    /// Drain this regularly. While `capacity` buffers wait here, the processor
    /// holds further buffer messages, and everything queued after them, rather
    /// than free memory on the audio thread.
    pub fn returned(&mut self) -> Option<Returned> {
        self.returned.pop().ok()
    }

    /// Commands and buffer messages refused because the ring was full.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    pub fn voices(&self) -> usize {
        self.shared.position.len()
    }

    /// Active head position in seconds, as of the last `process` call.
    ///
    /// # Panics
    /// If `voice` is out of range, as for all getters here.
    pub fn position(&self, voice: usize) -> f32 {
        f32::from_bits(self.shared.position[voice].load(Relaxed))
    }

    /// Reflects changes made on the audio thread, such as rec-once finishing.
    pub fn rec(&self, voice: usize) -> bool {
        self.shared.rec[voice].load(Relaxed)
    }

    pub fn play(&self, voice: usize) -> bool {
        self.shared.play[voice].load(Relaxed)
    }

    /// Both heads of `voice`, as of the last `process` call; see
    /// [`Voice::heads`](crate::Voice::heads).
    pub fn heads(&self, voice: usize) -> [HeadState; 2] {
        self.shared.heads[voice]
            .each_ref()
            .map(|(pos, fade, active)| {
                HeadState::new(
                    f32::from_bits(pos.load(Relaxed)),
                    f32::from_bits(fade.load(Relaxed)),
                    active.load(Relaxed),
                )
            })
    }
}

/// Audio-thread half: owns the engine. Allocation-, deallocation- and lock-free.
pub struct Processor {
    engine: Engine,
    rx: Consumer<Msg>,
    spent: Producer<Returned>,
    shared: Arc<Shared>,
}

impl Processor {
    /// Apply queued messages, process, publish state. Arguments as for
    /// [`Engine::process`].
    pub fn process(&mut self, input: &[f32], output: &mut [f32]) {
        self.process_with(input, output, |_, _| {});
    }

    /// [`process`](Self::process) with a per-voice insert, as
    /// [`Engine::process_with`].
    pub fn process_with(
        &mut self,
        input: &[f32],
        output: &mut [f32],
        insert: impl FnMut(usize, &mut [f32]),
    ) {
        self.apply_pending();
        self.engine.process_with(input, output, insert);
        self.publish();
    }

    fn apply_pending(&mut self) {
        while let Ok(msg) = self.rx.peek() {
            // A buffer message needs somewhere to send its buffer back;
            // without it, stop here and keep the queue's order.
            if !matches!(msg, Msg::Cmd(_)) && self.spent.is_full() {
                break;
            }
            let Ok(msg) = self.rx.pop() else { break };
            // The handle validated indices and lengths, so none of these fail.
            let back = match msg {
                Msg::Cmd(cmd) => {
                    self.engine.apply(cmd);
                    continue;
                }
                Msg::Load(buffer, data) => {
                    let (Ok(data) | Err(data)) = self.engine.replace_buffer(buffer, data);
                    Returned::Replaced { buffer, data }
                }
                Msg::Write {
                    buffer,
                    start,
                    data,
                    preserve,
                    mix,
                    fade,
                } => {
                    let sr = self.engine.sample_rate();
                    let frames = |t: f32| (t * sr).round().max(0.0) as usize;
                    let (start, fade) = (frames(start), frames(fade));
                    buffer::write(
                        self.engine.buffer_mut(buffer),
                        start,
                        &data,
                        preserve,
                        mix,
                        fade,
                    );
                    Returned::Written { buffer, data }
                }
                Msg::Snapshot(buffer, mut data) => {
                    let src = &self.engine.buffers()[buffer];
                    let n = src.len().min(data.len());
                    data[..n].copy_from_slice(&src[..n]);
                    Returned::Snapshot { buffer, data }
                }
            };
            let _ = self.spent.push(back);
        }
    }

    /// For reading buffers or voice state on the audio thread.
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Changes made here bypass the queue; use them on the audio thread only.
    pub fn engine_mut(&mut self) -> &mut Engine {
        &mut self.engine
    }

    fn publish(&mut self) {
        for (i, v) in self.engine.voices().iter().enumerate() {
            self.shared.position[i].store(v.position().to_bits(), Relaxed);
            self.shared.rec[i].store(v.rec(), Relaxed);
            self.shared.play[i].store(v.play(), Relaxed);
            for (slot, h) in self.shared.heads[i].iter().zip(v.heads()) {
                slot.0.store(h.position.to_bits(), Relaxed);
                slot.1.store(h.fade.to_bits(), Relaxed);
                slot.2.store(h.active, Relaxed);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EngineConfig, VoiceCmd};

    fn engine() -> Engine {
        Engine::new(EngineConfig {
            voices: 2,
            buffers: 1,
            buffer_frames: 1 << 14,
            block_size: 64,
            ..Default::default()
        })
    }

    #[test]
    fn commands_apply_on_process_and_state_is_published() {
        let (mut h, mut p) = split(engine(), 16);
        h.send(EngineCmd::Voice(1, VoiceCmd::LoopEnd(0.2))).unwrap();
        h.send(EngineCmd::Voice(1, VoiceCmd::Loop(true))).unwrap();
        h.send(EngineCmd::Voice(1, VoiceCmd::Play(true))).unwrap();
        assert!(!h.play(1), "applied before process");
        let mut out = [0.0; 480 * 2];
        p.process(&[0.0; 480], &mut out);
        assert!(h.play(1) && !h.play(0));
        let [a, b] = h.heads(1);
        assert!(a.active && a.gain == 1.0 && b.gain == 0.0, "{a:?} {b:?}");
        assert_eq!(a.position, h.position(1));
        assert!((h.position(1) - 0.01).abs() < 1e-4, "{}", h.position(1));
    }

    #[test]
    fn full_ring_returns_command_and_counts() {
        let (mut h, _p) = split(engine(), 1);
        let cmd = EngineCmd::Level(0, 0.5);
        h.send(cmd).unwrap();
        assert_eq!(h.send(cmd), Err(Full(cmd)));
        assert_eq!(h.dropped(), 1);
    }

    #[test]
    fn rec_once_completion_is_visible_to_handle() {
        let (mut h, mut p) = split(engine(), 16);
        for c in [
            VoiceCmd::LoopEnd(0.05),
            VoiceCmd::Loop(true),
            VoiceCmd::Play(true),
            VoiceCmd::CutTo(0.0),
            VoiceCmd::RecOnce(true),
        ] {
            h.send(EngineCmd::Voice(0, c)).unwrap();
        }
        let mut out = [0.0; 256 * 2];
        p.process(&[0.1; 256], &mut out);
        assert!(h.rec(0));
        let blocks = std::thread::spawn(move || {
            let mut blocks = 1;
            while p.engine().voices()[0].rec() {
                p.process(&[0.1; 256], &mut out);
                blocks += 1;
                assert!(blocks < 100, "rec_once never finished");
            }
            blocks
        })
        .join()
        .unwrap();
        assert!(blocks >= 9, "finished after {blocks} blocks");
        assert!(!h.rec(0));
        assert!(h.play(0));
    }

    fn buf(n: usize, x: f32) -> Box<[f32]> {
        vec![x; n].into_boxed_slice()
    }

    #[test]
    fn load_is_ordered_with_commands_and_returns_old_buffer() {
        let (mut h, mut p) = split(engine(), 16);
        h.send(EngineCmd::ClearBuffer(0)).unwrap();
        h.load(0, buf(1 << 10, 0.25)).unwrap();
        // Sent after the load, so it must clear the loaded data.
        h.send(EngineCmd::ClearBuffer(0)).unwrap();
        assert!(h.returned().is_none(), "returned before process");
        let mut out = [0.0; 64 * 2];
        p.process(&[0.0; 64], &mut out);
        let b = &p.engine().buffers()[0];
        assert_eq!(b.len(), 1 << 10);
        assert!(b.iter().all(|&x| x == 0.0));
        let back = h.returned().unwrap();
        assert!(matches!(back, Returned::Replaced { buffer: 0, .. }));
        assert_eq!(back.into_data().len(), 1 << 14);
        assert!(h.returned().is_none());
    }

    #[test]
    fn load_is_validated_on_the_control_thread() {
        let (mut h, _p) = split(engine(), 16);
        assert_eq!(h.load(1, buf(8, 0.0)), Err(BufferError::NoSuchBuffer(1)));
        assert_eq!(
            h.write(1, 0.0, buf(8, 0.0), 0.0, 1.0, 0.0),
            Err(BufferError::NoSuchBuffer(1))
        );
        assert_eq!(
            h.snapshot(1, buf(8, 0.0)),
            Err(BufferError::NoSuchBuffer(1))
        );
        assert_eq!(
            h.load(0, buf(100, 0.0)),
            Err(BufferError::NotPowerOfTwo(100))
        );
        assert_eq!(h.dropped(), 0);
    }

    #[test]
    fn full_return_ring_holds_loads_in_order() {
        let (mut h, mut p) = split(engine(), 1);
        let mut out = [0.0; 64 * 2];
        h.load(0, buf(8, 1.0)).unwrap();
        p.process(&[0.0; 64], &mut out);
        // The return ring now holds one buffer and is full.
        h.load(0, buf(16, 2.0)).unwrap();
        p.process(&[0.0; 64], &mut out);
        assert_eq!(
            p.engine().buffers()[0].len(),
            8,
            "load applied with nowhere to return"
        );
        assert!(matches!(h.send(EngineCmd::ClearBuffer(0)), Err(Full(_))));

        assert_eq!(h.returned().unwrap().into_data().len(), 1 << 14);
        p.process(&[0.0; 64], &mut out);
        assert_eq!(p.engine().buffers()[0].len(), 16);
        assert_eq!(h.returned().unwrap().into_data().len(), 8);
    }

    #[test]
    fn load_into_full_ring_returns_data() {
        let (mut h, _p) = split(engine(), 1);
        h.send(EngineCmd::Level(0, 1.0)).unwrap();
        match h.load(0, buf(8, 3.0)) {
            Err(BufferError::Full(d)) => assert_eq!(d.len(), 8),
            other => panic!("{other:?}"),
        }
        assert_eq!(h.dropped(), 1);
    }

    #[test]
    fn write_and_snapshot_round_trip_in_order() {
        let (mut h, mut p) = split(engine(), 16);
        let sr = p.engine().sample_rate();
        // Write 0.5 at 0.01 s, then snapshot, then clear: the snapshot must
        // see the write and not the clear.
        h.write(0, 0.01, buf(100, 0.5), 0.0, 1.0, 0.0).unwrap();
        h.snapshot(0, buf(1 << 14, -1.0)).unwrap();
        h.send(EngineCmd::ClearBuffer(0)).unwrap();
        let mut out = [0.0; 64 * 2];
        p.process(&[0.0; 64], &mut out);

        let start = (0.01 * sr) as usize;
        match h.returned().unwrap() {
            Returned::Written { buffer: 0, data } => assert_eq!(data.len(), 100),
            other => panic!("{other:?}"),
        }
        let snap = match h.returned().unwrap() {
            Returned::Snapshot { buffer: 0, data } => data,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            (
                snap[start - 1],
                snap[start],
                snap[start + 99],
                snap[start + 100]
            ),
            (0.0, 0.5, 0.5, 0.0)
        );
        assert!(p.engine().buffers()[0].iter().all(|&x| x == 0.0));
    }

    #[test]
    fn short_snapshot_copies_a_prefix() {
        let (mut h, mut p) = split(engine(), 16);
        p.engine_mut().buffer_mut(0).fill(0.25);
        h.snapshot(0, buf(10, 0.0)).unwrap();
        let mut out = [0.0; 64 * 2];
        p.process(&[0.0; 64], &mut out);
        assert_eq!(*h.returned().unwrap().into_data(), [0.25; 10]);
    }
}
