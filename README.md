# softcut-rs

[![CI](https://github.com/shakfu/softcut-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/shakfu/softcut-rs/actions/workflows/ci.yml)

Rust port of [softcut-lib](https://github.com/monome/softcut-lib), the looping engine behind monome norns. Modelled on [softcut-py](https://github.com/shakfu/softcut-py).

| Crate | Purpose | Dependencies |
|-|-|-|
| `softcut` | The DSP library, for embedding in a Rust audio host | none; `rtrb` optional |
| `softcut-demo` | egui app: live looping with the buffer, loops and playheads drawn | cpal, eframe, rtrb, hound, rfd |

## Library

- `Voice`: one crossfaded, resampling read/write head, with pre (input) and post (output) state-variable filters. It holds no buffer; `process_block(&mut buf, input, output)` borrows one per call. Voices share a buffer by being processed in turn.

- `Engine`: a multi-voice host. It owns the buffers, and adds per-voice level, pan, input gain and a voice-to-voice feedback matrix (one block of latency). It processes mono input into interleaved output.

- `VoiceCmd` / `EngineCmd`: `Copy` enums covering every setter. Send them over any SPSC queue and call `apply` on the audio thread.

Neither type allocates after construction, locks, or spawns threads. The host owns threading.

```rust
use softcut::{Engine, EngineCmd, EngineConfig, VoiceCmd};

let mut e = Engine::new(EngineConfig { voices: 2, buffers: 1, ..Default::default() });
e.apply(EngineCmd::Voice(0, VoiceCmd::LoopEnd(4.0)));
e.apply(EngineCmd::Voice(0, VoiceCmd::Loop(true)));
e.apply(EngineCmd::Voice(0, VoiceCmd::RecLevel(1.0)));
e.apply(EngineCmd::Voice(0, VoiceCmd::Rec(true)));
e.apply(EngineCmd::Voice(0, VoiceCmd::Play(true)));

let input = vec![0.0f32; 256];                       // mono
let mut output = vec![0.0f32; 256 * e.out_channels()]; // interleaved
e.process(&input, &mut output);
```

`make example` runs `softcut/examples/render.rs`, an offline render that reports the realtime factor: 6 voices run at about 140x realtime on an M-series Mac.

### Cross-thread control (feature `rtrb`)

`softcut::rt::split(engine, capacity)` returns a `Handle` for the control thread and a `Processor` for the audio thread. `Handle::send` queues an `EngineCmd` on a wait-free SPSC ring ([rtrb](https://docs.rs/rtrb)). When the ring is full, `send` returns the command and increments `dropped()`. `Processor::process` applies queued commands, processes, then publishes each voice's position and rec/play flags. The handle reads them back, including rec turning off when a rec-once pass ends.

`Handle::load(buffer, data)` replaces a buffer through the same ring, so it applies in order with commands. The new length may differ but must be a power of two. The swap is O(1). The old buffer comes back through `Handle::returned()` and is freed there or kept, e.g. to save a recording. The audio thread never frees memory: while replaced buffers wait unreturned, further loads, and everything queued behind them, are held. Without `rt`, `Engine::replace_buffer` does the same swap directly.

```rust
let (mut handle, mut processor) = softcut::rt::split(engine, 1024);
// audio callback: processor.process(&input, &mut output);
handle.send(EngineCmd::Voice(0, VoiceCmd::RecOnce(true)))?;
let pos = handle.position(0);
handle.load(0, samples)?;                    // Box<[f32]>, power-of-two length
while let Some((_, old)) = handle.returned() { drop(old) }
```

The ring has one producer. Hosts with several control sources must serialize them onto one `Handle`.

## Parity with the C++ engine

`softcut/tests/golden.rs` replays 8 scenarios recorded from softcut-lib through softcut-py. The scenarios cover recording, overdub, varispeed in both directions, filters, rec-once, one-shot, phase quantization and engine feedback. Output, buffer contents and head positions match within 1.2e-7. The exception is varispeed with rate slew, at 7.6e-5: clang fuses the slew update into an FMA on arm64 and Rust does not. `make fixtures` regenerates the fixtures; see `scripts/gen_fixtures.py`.

Upstream behaviour kept for parity:

- Recorded material is polarity-inverted. The "raised" rec fade curve computes `-sin(x)`.

- `Voice::reset` leaves a fade time of 0.1 s, not the 0.01 s it sets. The head's own init runs afterwards and overrides it.

- The input filter's cutoff tracks the rate at the moment it is set, not the rate as it slews.

Deviations, all where upstream behaviour is undefined:

- Rec without play outputs 0. Upstream reads an uninitialized local.

- Rates above 64 clamp the resampler's output frame count. Upstream writes past its 64-frame buffer.

- A non-power-of-two buffer panics. Upstream asserts in debug builds and indexes out of bounds in release builds.

## Demo

```sh
make demo
```

Four voices share one 43.7 s buffer (at 48 kHz). Voice 1 records the input when you press rec; voices 2-4 play the buffer back at other rates and directions. The input is the default microphone or off; the mic stream is paused while off, which is the startup state. "load wav..." or dropping a file on the window loads a WAV into the buffer: it is mixed to mono, linearly resampled to the device rate, and truncated to fit. Loading stops recording and loops every voice over the file. The waveform zooms to the sample on load; "fit sample" and "full buffer" switch between the two views. On the waveform, click to cut the selected voice, drag to set its loop. The microphone is opened at the output device's sample rate; if it does not support that rate, it is disabled and the status line says why.

## Development

```sh
make test   # unit, golden and doc tests, with and without the rtrb feature
make lint   # clippy -D warnings (both feature sets), rustfmt --check
```

CI (`.github/workflows/ci.yml`) runs the same checks on Linux and Windows (x86-64) and macOS (arm64), plus a rustdoc build with warnings as errors.

## License

GPL-3.0-only, as a derivative of [softcut-lib](https://github.com/monome/softcut-lib) (GPL-3.0). See `LICENSE`.
