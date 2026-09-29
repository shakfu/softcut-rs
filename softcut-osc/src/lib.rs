//! OSC control for [`softcut`], over the protocol of softcut-lib's reference
//! client, `softcut_jack_osc`: the messages norns sends to its audio engine.
//! Indices on the wire are 0-based.
//!
//! - [`parse`] turns one OSC message into [`Action`]s. Most are
//!   [`EngineCmd`]s; file reads and writes, the phase poll and reset are left
//!   to the host, which owns files and threads.
//! - [`Server`] receives UDP on its own thread and forwards parsed actions
//!   over a bounded channel. It never touches an engine, so the host applies
//!   actions through its one control path (e.g. a `softcut::rt::Handle`).
//! - [`PhasePoll`] reports quantized head positions as
//!   `/poll/softcut/phase i f`, as the reference does.
//!
//! This is the network edge: arguments are type-checked, numbers coerced
//! between int and float as the reference's liblo does, and non-finite
//! values rejected. [`DEFAULT_LISTEN`] is loopback only, because the
//! protocol writes files at paths the sender names.

use std::io;
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::thread::JoinHandle;
use std::time::Duration;

use rosc::{OscMessage, OscPacket, OscType};
use softcut::{EngineCmd, VoiceCmd};

/// The reference client's port, on loopback only.
pub const DEFAULT_LISTEN: &str = "127.0.0.1:9999";
/// Where the reference sends polls: sclang's default port.
pub const DEFAULT_REPLY: &str = "127.0.0.1:57120";
pub const PHASE_ADDRESS: &str = "/poll/softcut/phase";

/// What one OSC message asks for.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Engine(EngineCmd),
    /// Read a file channel into a buffer at `start_dst` seconds. A negative
    /// `dur` reads to the end.
    ReadMono {
        path: PathBuf,
        start_src: f32,
        start_dst: f32,
        dur: f32,
        ch_src: usize,
        ch_dst: usize,
    },
    /// Read a stereo file into buffers 0 and 1; a mono file fills both.
    ReadStereo {
        path: PathBuf,
        start_src: f32,
        start_dst: f32,
        dur: f32,
    },
    /// Write a region of one buffer to a mono file. A negative `dur` writes
    /// to the end.
    WriteMono {
        path: PathBuf,
        start: f32,
        dur: f32,
        ch: usize,
    },
    /// Write a region of buffers 0 and 1 to a stereo file.
    WriteStereo {
        path: PathBuf,
        start: f32,
        dur: f32,
    },
    /// Start or stop the phase poll.
    PhasePoll(bool),
    /// Reset every voice. Sent with buffer clears and a poll stop, as the
    /// reference resets; what else to restore is the host's choice.
    Reset,
    /// Accepted for compatibility; has no effect here.
    Ignored(&'static str),
}

#[derive(Clone, Debug, PartialEq)]
pub enum ParseError {
    UnknownAddress(String),
    /// Missing or mistyped arguments; `expected` is the OSC type tag string.
    BadArgs {
        addr: String,
        expected: &'static str,
    },
    /// A NaN or infinite number, which would corrupt the engine's state.
    NotFinite(String),
    /// A datagram that is not an OSC packet.
    Undecodable,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::UnknownAddress(a) => write!(f, "unknown OSC address {a}"),
            ParseError::BadArgs { addr, expected } => {
                write!(f, "{addr} expects arguments {expected}")
            }
            ParseError::NotFinite(a) => write!(f, "{a}: non-finite number"),
            ParseError::Undecodable => write!(f, "undecodable OSC packet"),
        }
    }
}

impl std::error::Error for ParseError {}

/// Typed access to a message's arguments.
struct Args<'a> {
    msg: &'a OscMessage,
    expected: &'static str,
}

impl Args<'_> {
    fn bad(&self) -> ParseError {
        ParseError::BadArgs {
            addr: self.msg.addr.clone(),
            expected: self.expected,
        }
    }

    fn f(&self, k: usize) -> Result<f32, ParseError> {
        let x = match self.msg.args.get(k) {
            Some(OscType::Float(x)) => *x,
            Some(OscType::Double(x)) => *x as f32,
            Some(OscType::Int(x)) => *x as f32,
            Some(OscType::Long(x)) => *x as f32,
            Some(OscType::Bool(b)) => f32::from(u8::from(*b)),
            _ => return Err(self.bad()),
        };
        if x.is_finite() {
            Ok(x)
        } else {
            Err(ParseError::NotFinite(self.msg.addr.clone()))
        }
    }

    /// A non-negative integer; integral floats are accepted.
    fn i(&self, k: usize) -> Result<usize, ParseError> {
        let x = match self.msg.args.get(k) {
            Some(OscType::Int(x)) => *x as f64,
            Some(OscType::Long(x)) => *x as f64,
            Some(OscType::Float(x)) => *x as f64,
            Some(OscType::Double(x)) => *x,
            _ => return Err(self.bad()),
        };
        if x >= 0.0 && x.fract() == 0.0 && x < u32::MAX as f64 {
            Ok(x as usize)
        } else {
            Err(self.bad())
        }
    }

    fn s(&self, k: usize) -> Result<PathBuf, ParseError> {
        match self.msg.args.get(k) {
            Some(OscType::String(s)) if !s.is_empty() => Ok(PathBuf::from(s)),
            _ => Err(self.bad()),
        }
    }

    /// Optional trailing float, as the reference's variable-length messages.
    fn f_or(&self, k: usize, default: f32) -> Result<f32, ParseError> {
        if k < self.msg.args.len() {
            self.f(k)
        } else {
            Ok(default)
        }
    }

    fn i_or(&self, k: usize, default: usize) -> Result<usize, ParseError> {
        if k < self.msg.args.len() {
            self.i(k)
        } else {
            Ok(default)
        }
    }
}

/// A `/set/param/cut/<name> i f` setting.
fn voice_param(name: &str, x: f32) -> Option<VoiceCmd> {
    use VoiceCmd::*;
    let on = x > 0.0;
    Some(match name {
        "rate" => Rate(x),
        "loop_start" => LoopStart(x),
        "loop_end" => LoopEnd(x),
        "loop_flag" => Loop(on),
        "fade_time" => FadeTime(x),
        "rec_level" => RecLevel(x),
        "pre_level" => PreLevel(x),
        "rec_flag" => Rec(on),
        "rec_once" => RecOnce(on),
        "play_flag" => Play(on),
        "rec_offset" => RecOffset(x),
        "position" => CutTo(x),
        "recpre_slew_time" => RecPreSlewTime(x),
        "rate_slew_time" => RateSlewTime(x),
        "phase_quant" => PhaseQuant(x),
        "phase_offset" => PhaseOffset(x),
        "pre_filter_fc" => PreFilterFc(x),
        "pre_filter_fc_mod" => PreFilterFcMod(x),
        "pre_filter_rq" => PreFilterRq(x),
        "pre_filter_lp" => PreFilterLp(x),
        "pre_filter_hp" => PreFilterHp(x),
        "pre_filter_bp" => PreFilterBp(x),
        "pre_filter_br" => PreFilterBr(x),
        "pre_filter_dry" => PreFilterDry(x),
        "post_filter_fc" => PostFilterFc(x),
        "post_filter_rq" => PostFilterRq(x),
        "post_filter_lp" => PostFilterLp(x),
        "post_filter_hp" => PostFilterHp(x),
        "post_filter_bp" => PostFilterBp(x),
        "post_filter_br" => PostFilterBr(x),
        "post_filter_dry" => PostFilterDry(x),
        _ => return None,
    })
}

/// The actions one message asks for, in order.
pub fn parse(msg: &OscMessage) -> Result<Vec<Action>, ParseError> {
    use Action::Engine as E;
    let args = |expected| Args { msg, expected };
    let addr = msg.addr.as_str();

    if let Some(name) = addr.strip_prefix("/set/param/cut/") {
        return match name {
            "voice_sync" => {
                let a = args("iif");
                Ok(vec![E(EngineCmd::Sync {
                    follow: a.i(0)?,
                    lead: a.i(1)?,
                    offset: a.f(2)?,
                })])
            }
            "buffer" => {
                let a = args("ii");
                Ok(vec![E(EngineCmd::VoiceBuffer(a.i(0)?, a.i(1)?))])
            }
            "level_slew_time" | "pan_slew_time" => {
                let a = args("if");
                a.i(0)?;
                a.f(1)?;
                Ok(vec![Action::Ignored("the engine has no level or pan slew")])
            }
            _ => {
                let a = args("if");
                let (voice, x) = (a.i(0)?, a.f(1)?);
                let cmd =
                    voice_param(name, x).ok_or_else(|| ParseError::UnknownAddress(addr.into()))?;
                Ok(vec![E(EngineCmd::Voice(voice, cmd))])
            }
        };
    }

    let actions = match addr {
        "/set/level/cut" => {
            let a = args("if");
            vec![E(EngineCmd::Level(a.i(0)?, a.f(1)?))]
        }
        "/set/pan/cut" => {
            let a = args("if");
            vec![E(EngineCmd::Pan(a.i(0)?, a.f(1)?))]
        }
        "/set/level/in_cut" => {
            let a = args("iif");
            vec![E(EngineCmd::InputLevel {
                channel: a.i(0)?,
                voice: a.i(1)?,
                amount: a.f(2)?,
            })]
        }
        "/set/level/cut_cut" => {
            let a = args("iif");
            vec![E(EngineCmd::Feedback {
                src: a.i(0)?,
                dst: a.i(1)?,
                amount: a.f(2)?,
            })]
        }
        "/set/enabled/cut" => {
            let a = args("if");
            a.i(0)?;
            a.f(1)?;
            vec![Action::Ignored("voices are always enabled")]
        }
        "/softcut/buffer/read_mono" => {
            let a = args("s[fffii]");
            vec![Action::ReadMono {
                path: a.s(0)?,
                start_src: a.f_or(1, 0.0)?,
                start_dst: a.f_or(2, 0.0)?,
                dur: a.f_or(3, -1.0)?,
                ch_src: a.i_or(4, 0)?,
                ch_dst: a.i_or(5, 0)?,
            }]
        }
        "/softcut/buffer/read_stereo" => {
            let a = args("s[fff]");
            vec![Action::ReadStereo {
                path: a.s(0)?,
                start_src: a.f_or(1, 0.0)?,
                start_dst: a.f_or(2, 0.0)?,
                dur: a.f_or(3, -1.0)?,
            }]
        }
        "/softcut/buffer/write_mono" => {
            let a = args("s[ffi]");
            vec![Action::WriteMono {
                path: a.s(0)?,
                start: a.f_or(1, 0.0)?,
                dur: a.f_or(2, -1.0)?,
                ch: a.i_or(3, 0)?,
            }]
        }
        "/softcut/buffer/write_stereo" => {
            let a = args("s[ff]");
            vec![Action::WriteStereo {
                path: a.s(0)?,
                start: a.f_or(1, 0.0)?,
                dur: a.f_or(2, -1.0)?,
            }]
        }
        "/softcut/buffer/clear" => vec![E(EngineCmd::ClearBuffer(0)), E(EngineCmd::ClearBuffer(1))],
        "/softcut/buffer/clear_channel" => vec![E(EngineCmd::ClearBuffer(args("i").i(0)?))],
        "/softcut/buffer/clear_region" => {
            let a = args("ff");
            let (start, len) = (a.f(0)?, a.f(1)?);
            (0..2)
                .map(|buffer| {
                    E(EngineCmd::ClearRegion {
                        buffer,
                        start,
                        len,
                        fade: 0.0,
                        preserve: 0.0,
                    })
                })
                .collect()
        }
        "/softcut/buffer/clear_region_channel" => {
            let a = args("iff");
            vec![E(EngineCmd::ClearRegion {
                buffer: a.i(0)?,
                start: a.f(1)?,
                len: a.f(2)?,
                fade: 0.0,
                preserve: 0.0,
            })]
        }
        "/softcut/reset" => vec![
            E(EngineCmd::ClearBuffer(0)),
            E(EngineCmd::ClearBuffer(1)),
            Action::Reset,
            Action::PhasePoll(false),
        ],
        "/poll/start/cut/phase" => vec![Action::PhasePoll(true)],
        "/poll/stop/cut/phase" => vec![Action::PhasePoll(false)],
        "/hello" | "/goodbye" | "/poll/start/vu" | "/poll/stop/vu" => {
            vec![Action::Ignored("no effect here")]
        }
        "/quit" => vec![Action::Ignored("a network message does not quit the host")],
        _ => return Err(ParseError::UnknownAddress(addr.into())),
    };
    Ok(actions)
}

/// Every message in a packet, depth first.
fn messages(packet: OscPacket, out: &mut Vec<OscMessage>) {
    match packet {
        OscPacket::Message(m) => out.push(m),
        OscPacket::Bundle(b) => b.content.into_iter().for_each(|p| messages(p, out)),
    }
}

/// Parse one UDP datagram, which may hold a bundle.
pub fn parse_packet(data: &[u8]) -> Vec<Result<Action, ParseError>> {
    let Ok((_, packet)) = rosc::decoder::decode_udp(data) else {
        return vec![Err(ParseError::Undecodable)];
    };
    let mut msgs = Vec::new();
    messages(packet, &mut msgs);
    msgs.iter()
        .flat_map(|m| match parse(m) {
            Ok(actions) => actions.into_iter().map(Ok).collect(),
            Err(e) => vec![Err(e)],
        })
        .collect()
}

/// Actions waiting for the host; more are dropped and counted.
const QUEUE: usize = 4096;

/// Receives OSC over UDP on a background thread. Dropping it stops the thread.
pub struct Server {
    rx: Receiver<Result<Action, ParseError>>,
    socket: UdpSocket,
    reply_to: SocketAddr,
    dropped: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Server {
    /// Bind `listen` and start receiving; polls go to `reply_to`.
    pub fn start(listen: impl ToSocketAddrs, reply_to: SocketAddr) -> io::Result<Self> {
        let socket = UdpSocket::bind(listen)?;
        // Wakes the thread to check `stop`.
        socket.set_read_timeout(Some(Duration::from_millis(50)))?;
        let (tx, rx) = sync_channel(QUEUE);
        let (dropped, stop) = (
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicBool::new(false)),
        );
        let thread = {
            let (socket, dropped, stop) = (socket.try_clone()?, dropped.clone(), stop.clone());
            std::thread::Builder::new()
                .name("softcut-osc".into())
                .spawn(move || receive(socket, tx, dropped, stop))?
        };
        Ok(Self {
            rx,
            socket,
            reply_to,
            dropped,
            stop,
            thread: Some(thread),
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// Next parsed action or error, oldest first.
    pub fn try_recv(&self) -> Option<Result<Action, ParseError>> {
        self.rx.try_recv().ok()
    }

    /// Actions dropped because the host was not draining them.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Relaxed)
    }

    /// Send a message to the reply address.
    pub fn send(&self, msg: OscMessage) -> io::Result<()> {
        let data = rosc::encoder::encode(&OscPacket::Message(msg))
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        self.socket.send_to(&data, self.reply_to).map(|_| ())
    }

    /// Send `/poll/softcut/phase voice phase`.
    pub fn send_phase(&self, voice: usize, phase: f32) -> io::Result<()> {
        self.send(OscMessage {
            addr: PHASE_ADDRESS.into(),
            args: vec![OscType::Int(voice as i32), OscType::Float(phase)],
        })
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn receive(
    socket: UdpSocket,
    tx: SyncSender<Result<Action, ParseError>>,
    dropped: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
) {
    let mut buf = vec![0u8; 65536];
    while !stop.load(Relaxed) {
        let Ok((n, _)) = socket.recv_from(&mut buf) else {
            continue; // timeout, or a transient error
        };
        for item in parse_packet(&buf[..n]) {
            match tx.try_send(item) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    dropped.fetch_add(1, Relaxed);
                }
                Err(TrySendError::Disconnected(_)) => return,
            }
        }
    }
}

/// The reference's phase poll: reports each voice's quantized position when
/// it changes. Feed it every action with [`observe`](Self::observe), since
/// the quantization settings arrive as ordinary voice commands.
pub struct PhasePoll {
    on: bool,
    quant: Vec<f32>,
    offset: Vec<f32>,
    last: Vec<Option<f32>>,
}

impl PhasePoll {
    pub fn new(voices: usize) -> Self {
        Self {
            on: false,
            quant: vec![0.0; voices],
            offset: vec![0.0; voices],
            last: vec![None; voices],
        }
    }

    pub fn is_on(&self) -> bool {
        self.on
    }

    pub fn observe(&mut self, action: &Action) {
        match action {
            Action::PhasePoll(on) => {
                self.on = *on;
                self.last.fill(None);
            }
            Action::Reset => {
                self.quant.fill(0.0);
                self.offset.fill(0.0);
            }
            Action::Engine(EngineCmd::Voice(v, VoiceCmd::PhaseQuant(q)))
                if *v < self.quant.len() =>
            {
                self.quant[*v] = *q
            }
            Action::Engine(EngineCmd::Voice(v, VoiceCmd::PhaseOffset(o)))
                if *v < self.offset.len() =>
            {
                self.offset[*v] = *o
            }
            _ => {}
        }
    }

    /// Voices whose quantized phase changed since the last call, given each
    /// voice's position in seconds. Empty while the poll is off.
    pub fn changed(&mut self, position: impl Fn(usize) -> f32) -> Vec<(usize, f32)> {
        if !self.on {
            return Vec::new();
        }
        let mut out = Vec::new();
        for v in 0..self.last.len() {
            let (q, pos) = (self.quant[v], position(v));
            // As softcut's quantized phase: floor((pos + offset) / q) * q.
            let phase = if q == 0.0 {
                pos
            } else {
                ((pos + self.offset[v]) / q).floor() * q
            };
            if self.last[v] != Some(phase) {
                self.last[v] = Some(phase);
                out.push((v, phase));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(addr: &str, args: Vec<OscType>) -> OscMessage {
        OscMessage {
            addr: addr.into(),
            args,
        }
    }

    use OscType::{Float as F, Int as I, String as S};

    #[test]
    fn every_voice_param_maps_to_its_command() {
        use VoiceCmd::*;
        let cases = [
            ("rate", Rate(1.5)),
            ("loop_start", LoopStart(1.5)),
            ("loop_end", LoopEnd(1.5)),
            ("loop_flag", Loop(true)),
            ("fade_time", FadeTime(1.5)),
            ("rec_level", RecLevel(1.5)),
            ("pre_level", PreLevel(1.5)),
            ("rec_flag", Rec(true)),
            ("rec_once", RecOnce(true)),
            ("play_flag", Play(true)),
            ("rec_offset", RecOffset(1.5)),
            ("position", CutTo(1.5)),
            ("recpre_slew_time", RecPreSlewTime(1.5)),
            ("rate_slew_time", RateSlewTime(1.5)),
            ("phase_quant", PhaseQuant(1.5)),
            ("phase_offset", PhaseOffset(1.5)),
            ("pre_filter_fc", PreFilterFc(1.5)),
            ("pre_filter_fc_mod", PreFilterFcMod(1.5)),
            ("pre_filter_rq", PreFilterRq(1.5)),
            ("pre_filter_lp", PreFilterLp(1.5)),
            ("pre_filter_hp", PreFilterHp(1.5)),
            ("pre_filter_bp", PreFilterBp(1.5)),
            ("pre_filter_br", PreFilterBr(1.5)),
            ("pre_filter_dry", PreFilterDry(1.5)),
            ("post_filter_fc", PostFilterFc(1.5)),
            ("post_filter_rq", PostFilterRq(1.5)),
            ("post_filter_lp", PostFilterLp(1.5)),
            ("post_filter_hp", PostFilterHp(1.5)),
            ("post_filter_bp", PostFilterBp(1.5)),
            ("post_filter_br", PostFilterBr(1.5)),
            ("post_filter_dry", PostFilterDry(1.5)),
        ];
        for (name, cmd) in cases {
            let m = msg(&format!("/set/param/cut/{name}"), vec![I(3), F(1.5)]);
            assert_eq!(
                parse(&m),
                Ok(vec![Action::Engine(EngineCmd::Voice(3, cmd))]),
                "{name}"
            );
        }
        let off = msg("/set/param/cut/play_flag", vec![I(0), F(0.0)]);
        assert_eq!(
            parse(&off),
            Ok(vec![Action::Engine(EngineCmd::Voice(0, Play(false)))])
        );
    }

    #[test]
    fn routing_and_mix_messages() {
        let cases = [
            (
                msg("/set/level/cut", vec![I(1), F(0.5)]),
                EngineCmd::Level(1, 0.5),
            ),
            (
                msg("/set/pan/cut", vec![I(1), F(-0.5)]),
                EngineCmd::Pan(1, -0.5),
            ),
            (
                msg("/set/level/in_cut", vec![I(1), I(2), F(0.5)]),
                EngineCmd::InputLevel {
                    channel: 1,
                    voice: 2,
                    amount: 0.5,
                },
            ),
            (
                msg("/set/level/cut_cut", vec![I(0), I(3), F(0.25)]),
                EngineCmd::Feedback {
                    src: 0,
                    dst: 3,
                    amount: 0.25,
                },
            ),
            (
                msg("/set/param/cut/voice_sync", vec![I(1), I(0), F(0.1)]),
                EngineCmd::Sync {
                    follow: 1,
                    lead: 0,
                    offset: 0.1,
                },
            ),
            (
                msg("/set/param/cut/buffer", vec![I(2), I(1)]),
                EngineCmd::VoiceBuffer(2, 1),
            ),
            (
                msg("/softcut/buffer/clear_channel", vec![I(1)]),
                EngineCmd::ClearBuffer(1),
            ),
            (
                msg(
                    "/softcut/buffer/clear_region_channel",
                    vec![I(1), F(0.5), F(2.0)],
                ),
                EngineCmd::ClearRegion {
                    buffer: 1,
                    start: 0.5,
                    len: 2.0,
                    fade: 0.0,
                    preserve: 0.0,
                },
            ),
        ];
        for (m, cmd) in cases {
            assert_eq!(parse(&m), Ok(vec![Action::Engine(cmd)]), "{}", m.addr);
        }
        assert_eq!(
            parse(&msg("/softcut/buffer/clear_region", vec![F(0.5), F(2.0)]))
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn file_messages_take_optional_arguments() {
        let short = msg("/softcut/buffer/read_mono", vec![S("/tmp/a.wav".into())]);
        assert_eq!(
            parse(&short),
            Ok(vec![Action::ReadMono {
                path: "/tmp/a.wav".into(),
                start_src: 0.0,
                start_dst: 0.0,
                dur: -1.0,
                ch_src: 0,
                ch_dst: 0,
            }])
        );
        let full = msg(
            "/softcut/buffer/write_mono",
            vec![S("/tmp/b.wav".into()), F(1.0), F(2.0), I(1)],
        );
        assert_eq!(
            parse(&full),
            Ok(vec![Action::WriteMono {
                path: "/tmp/b.wav".into(),
                start: 1.0,
                dur: 2.0,
                ch: 1
            }])
        );
        let no_path = msg("/softcut/buffer/write_stereo", vec![]);
        assert!(matches!(parse(&no_path), Err(ParseError::BadArgs { .. })));
    }

    #[test]
    fn numbers_coerce_but_non_finite_and_bad_indices_are_rejected() {
        // An int where a float is due, and an integral float as an index.
        let coerced = msg("/set/param/cut/rate", vec![F(2.0), I(2)]);
        assert_eq!(
            parse(&coerced),
            Ok(vec![Action::Engine(EngineCmd::Voice(
                2,
                VoiceCmd::Rate(2.0)
            ))])
        );
        let nan = msg("/set/param/cut/rate", vec![I(0), F(f32::NAN)]);
        assert!(matches!(parse(&nan), Err(ParseError::NotFinite(_))));
        let inf = msg("/set/level/cut", vec![I(0), F(f32::INFINITY)]);
        assert!(matches!(parse(&inf), Err(ParseError::NotFinite(_))));
        for bad in [I(-1), F(0.5), S("x".into())] {
            let m = msg("/set/param/cut/rate", vec![bad, F(1.0)]);
            assert!(matches!(parse(&m), Err(ParseError::BadArgs { .. })));
        }
        let short = msg("/set/level/in_cut", vec![I(0), F(1.0)]);
        assert!(matches!(parse(&short), Err(ParseError::BadArgs { .. })));
    }

    #[test]
    fn unknown_and_ignored_addresses() {
        assert!(matches!(
            parse(&msg("/nope", vec![])),
            Err(ParseError::UnknownAddress(_))
        ));
        assert!(matches!(
            parse(&msg("/set/param/cut/nope", vec![I(0), F(1.0)])),
            Err(ParseError::UnknownAddress(_))
        ));
        for m in [
            msg("/quit", vec![]),
            msg("/set/enabled/cut", vec![I(0), F(1.0)]),
            msg("/set/param/cut/level_slew_time", vec![I(0), F(0.1)]),
        ] {
            assert!(
                matches!(parse(&m).unwrap()[..], [Action::Ignored(_)]),
                "{}",
                m.addr
            );
        }
    }

    #[test]
    fn server_receives_bundles_over_udp_and_replies() {
        let reply = UdpSocket::bind("127.0.0.1:0").unwrap();
        reply
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let server = Server::start("127.0.0.1:0", reply.local_addr().unwrap()).unwrap();
        let bundle = OscPacket::Bundle(rosc::OscBundle {
            timetag: rosc::OscTime {
                seconds: 0,
                fractional: 1,
            },
            content: vec![
                OscPacket::Message(msg("/set/param/cut/rate", vec![I(0), F(-1.0)])),
                OscPacket::Message(msg("/bogus", vec![])),
            ],
        });
        let client = UdpSocket::bind("127.0.0.1:0").unwrap();
        client
            .send_to(
                &rosc::encoder::encode(&bundle).unwrap(),
                server.local_addr().unwrap(),
            )
            .unwrap();
        let mut got = Vec::new();
        for _ in 0..200 {
            got.extend(std::iter::from_fn(|| server.try_recv()));
            if got.len() == 2 {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            got[0],
            Ok(Action::Engine(EngineCmd::Voice(0, VoiceCmd::Rate(-1.0))))
        );
        assert!(matches!(got[1], Err(ParseError::UnknownAddress(_))));

        server.send_phase(2, 0.25).unwrap();
        let mut buf = [0u8; 256];
        let (n, _) = reply.recv_from(&mut buf).unwrap();
        let (_, packet) = rosc::decoder::decode_udp(&buf[..n]).unwrap();
        assert_eq!(
            packet,
            OscPacket::Message(msg(PHASE_ADDRESS, vec![I(2), F(0.25)]))
        );
    }

    #[test]
    fn phase_poll_quantizes_and_reports_changes_only() {
        let mut poll = PhasePoll::new(2);
        assert!(poll.changed(|_| 1.0).is_empty(), "off by default");
        poll.observe(&Action::PhasePoll(true));
        poll.observe(&Action::Engine(EngineCmd::Voice(
            0,
            VoiceCmd::PhaseQuant(0.25),
        )));
        poll.observe(&Action::Engine(EngineCmd::Voice(
            0,
            VoiceCmd::PhaseOffset(0.1),
        )));
        // Voice 0: floor((0.3 + 0.1) / 0.25) * 0.25 = 0.25. Voice 1 unquantized.
        assert_eq!(poll.changed(|_| 0.3), vec![(0, 0.25), (1, 0.3)]);
        // Within the same quantum and position: nothing new.
        assert_eq!(poll.changed(|v| if v == 0 { 0.35 } else { 0.3 }), vec![]);
        assert_eq!(
            poll.changed(|v| if v == 0 { 0.41 } else { 0.3 }),
            vec![(0, 0.5)]
        );
        poll.observe(&Action::PhasePoll(false));
        assert!(poll.changed(|_| 9.0).is_empty());
    }
}
