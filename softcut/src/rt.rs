//! Cross-thread control of an [`Engine`] (feature `rtrb`).
//!
//! [`split`] divides an engine into a [`Handle`] for the control thread and a
//! [`Processor`] for the audio thread. Commands and buffer loads travel over
//! one wait-free SPSC ring, so they apply in the order they were sent, at the
//! start of the next [`Processor::process`] call. Head positions and rec/play
//! flags come back through atomics, published once per call. Replaced buffers
//! come back over a second ring, so the audio thread never frees memory.
//!
//! The ring has one producer. Hosts with several control sources must
//! serialize them onto the one `Handle`.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};

use rtrb::{Consumer, Producer, RingBuffer};

use crate::{Engine, EngineCmd};

enum Msg {
    Cmd(EngineCmd),
    Load(usize, Box<[f32]>),
}

struct Shared {
    /// Seconds, as f32 bits.
    position: Box<[AtomicU32]>,
    rec: Box<[AtomicBool]>,
    play: Box<[AtomicBool]>,
}

/// Returns the control half and the audio half of `engine`. `capacity` is the
/// number of messages that can be queued between two `process` calls, and
/// the number of replaced buffers that can wait for [`Handle::returned`].
pub fn split(engine: Engine, capacity: usize) -> (Handle, Processor) {
    let n = engine.voices().len();
    let buffers = engine.buffers().len();
    let shared = Arc::new(Shared {
        position: (0..n).map(|_| AtomicU32::new(0)).collect(),
        rec: (0..n).map(|_| AtomicBool::new(false)).collect(),
        play: (0..n).map(|_| AtomicBool::new(false)).collect(),
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

/// Why [`Handle::load`] refused a buffer.
#[derive(Debug, PartialEq)]
pub enum LoadError {
    /// No buffer has this index.
    NoSuchBuffer(usize),
    /// The length, which is not a power of two.
    NotPowerOfTwo(usize),
    /// The ring was full; the data is returned for a later retry.
    Full(Box<[f32]>),
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoadError::NoSuchBuffer(i) => write!(f, "no softcut buffer {i}"),
            LoadError::NotPowerOfTwo(n) => write!(f, "buffer length {n} is not a power of two"),
            LoadError::Full(_) => write!(f, "softcut command ring full"),
        }
    }
}

impl std::error::Error for LoadError {}

/// Control-thread half: sends commands and buffers, reads published state.
/// Never blocks.
pub struct Handle {
    tx: Producer<Msg>,
    returned: Consumer<(usize, Box<[f32]>)>,
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
    pub fn load(&mut self, buffer: usize, data: Box<[f32]>) -> Result<(), LoadError> {
        if buffer >= self.buffers {
            return Err(LoadError::NoSuchBuffer(buffer));
        }
        if !data.len().is_power_of_two() {
            return Err(LoadError::NotPowerOfTwo(data.len()));
        }
        self.tx.push(Msg::Load(buffer, data)).map_err(|e| {
            self.dropped += 1;
            match e {
                rtrb::PushError::Full(Msg::Load(_, data)) => LoadError::Full(data),
                rtrb::PushError::Full(Msg::Cmd(_)) => unreachable!(),
            }
        })
    }

    /// Next replaced buffer and its index, oldest first.
    ///
    /// Drain this regularly. While `capacity` buffers wait here, the processor
    /// holds further loads, and everything queued after them, rather than free
    /// memory on the audio thread.
    pub fn returned(&mut self) -> Option<(usize, Box<[f32]>)> {
        self.returned.pop().ok()
    }

    /// Commands and loads refused because the ring was full.
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
}

/// Audio-thread half: owns the engine. Allocation-, deallocation- and lock-free.
pub struct Processor {
    engine: Engine,
    rx: Consumer<Msg>,
    spent: Producer<(usize, Box<[f32]>)>,
    shared: Arc<Shared>,
}

impl Processor {
    /// Apply queued messages, process, publish state. Arguments as for
    /// [`Engine::process`].
    pub fn process(&mut self, input: &[f32], output: &mut [f32]) {
        while let Ok(msg) = self.rx.peek() {
            // A load needs somewhere to put the old buffer; without it, stop
            // here and keep the queue's order.
            if matches!(msg, Msg::Load(..)) && self.spent.is_full() {
                break;
            }
            match self.rx.pop() {
                Ok(Msg::Cmd(cmd)) => self.engine.apply(cmd),
                Ok(Msg::Load(i, data)) => {
                    // `Handle::load` validated both index and length.
                    if let Ok(old) = self.engine.replace_buffer(i, data) {
                        let _ = self.spent.push((i, old));
                    }
                }
                Err(_) => break,
            }
        }
        self.engine.process(input, output);
        self.publish();
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
        let (i, old) = h.returned().unwrap();
        assert_eq!((i, old.len()), (0, 1 << 14));
        assert!(h.returned().is_none());
    }

    #[test]
    fn load_is_validated_on_the_control_thread() {
        let (mut h, _p) = split(engine(), 16);
        assert_eq!(h.load(1, buf(8, 0.0)), Err(LoadError::NoSuchBuffer(1)));
        assert_eq!(h.load(0, buf(100, 0.0)), Err(LoadError::NotPowerOfTwo(100)));
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

        assert_eq!(h.returned().unwrap().1.len(), 1 << 14);
        p.process(&[0.0; 64], &mut out);
        assert_eq!(p.engine().buffers()[0].len(), 16);
        assert_eq!(h.returned().unwrap().1.len(), 8);
    }

    #[test]
    fn load_into_full_ring_returns_data() {
        let (mut h, _p) = split(engine(), 1);
        h.send(EngineCmd::Level(0, 1.0)).unwrap();
        match h.load(0, buf(8, 3.0)) {
            Err(LoadError::Full(d)) => assert_eq!(d.len(), 8),
            other => panic!("{other:?}"),
        }
        assert_eq!(h.dropped(), 1);
    }
}
