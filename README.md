# softcut-rs

[![CI](https://github.com/shakfu/softcut-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/shakfu/softcut-rs/actions/workflows/ci.yml)

Rust port of [softcut-lib](https://github.com/monome/softcut-lib), the looping engine behind monome norns. Modelled on [softcut-py](https://github.com/shakfu/softcut-py).

| Crate | Purpose | Dependencies |
|-|-|-|
| `softcut` | The DSP library, for embedding in a Rust audio host | none; `rtrb` optional |
| `softcut-demo` | egui app: stereo live looping with the buffers, loops and playheads drawn | cpal, eframe, rtrb, hound, rfd |

## Library

- `Voice`: one crossfaded, resampling read/write head, with pre (input) and post (output) state-variable filters. It holds no buffer; `process_block(&mut buf, input, output)` borrows one per call. Voices share a buffer by being processed in turn. The crossfade's rec and pre curves take a `FadeShape` (linear, sine, raised) and a delay/window ratio, as in softcut-lib's `FadeCurves`, which upstream's `Voice` does not expose.

- `Engine`: a multi-voice host. It owns the buffers, and adds per-voice level, pan, input gain and a voice-to-voice feedback matrix (one block of latency). It processes interleaved input of `in_channels` into interleaved output. An input level matrix (`EngineCmd::InputLevel`, norns `level_input_cut`) routes channels to voices; voice `v` starts on channel `v % in_channels`. Stereo, as on norns, is two voices on two buffers, panned apart.

- `buffer`: norns buffer operations on plain slices: `write` (a file read, once decoded), `clear` and `copy`/`copy_within`, each a blended write with edge fades. `EngineCmd::ClearRegion` and `CopyRegion` run them on the audio thread. A reversed copy between partly overlapping regions of one buffer needs a temporary copy, so it is refused rather than allocating.

- `VoiceCmd` / `EngineCmd`: `Copy` enums covering every setter. Send them over any SPSC queue and call `apply` on the audio thread.

Neither type allocates after construction, locks, or spawns threads. The host owns threading.

Three upstream quirks are kept by default:

- Recorded material is polarity-inverted. The "raised" rec fade curve computes `-sin(x)`.
- `Voice::reset` leaves a 0.1 s fade time, not the 0.01 s it sets.
- A raised pre fade shape only applies while the rec shape is also raised. Upstream tests the rec shape where it means the pre shape.

`Quirks::Fixed`, passed to `Voice::with_quirks` or `EngineConfig::quirks`, corrects all three. It is a constructor argument, not a Cargo feature: features unify across a dependency graph, so one crate enabling it would change every other crate's output.

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

Buffer transfers go through the same ring, so they apply in order with commands:

- `Handle::load(buffer, data)` swaps in a new buffer, O(1). The length may differ but must be a power of two. Without `rt`, `Engine::replace_buffer` does the same.
- `Handle::write(buffer, start, data, preserve, mix, fade)` blends data into a buffer, e.g. a decoded file (norns `buffer_read`).
- `Handle::snapshot(buffer, dest)` copies a buffer out between two blocks, consistent even while voices record. Use it to save a loop (norns `buffer_write`).

Every buffer sent comes back through `Handle::returned()` as a `Returned` (`Replaced`, `Written` or `Snapshot`), to be freed or kept there. The audio thread never frees memory: while returned buffers wait unclaimed, further buffer messages, and everything queued behind them, are held.

```rust
let (mut handle, mut processor) = softcut::rt::split(engine, 1024);
// audio callback: processor.process(&input, &mut output);
handle.send(EngineCmd::Voice(0, VoiceCmd::RecOnce(true)))?;
let pos = handle.position(0);
handle.load(0, samples)?;                    // Box<[f32]>, power-of-two length
handle.snapshot(0, vec![0.0; n].into())?;    // copy out the first n frames
while let Some(r) = handle.returned() {
    if let Returned::Snapshot { data, .. } = r { save(&data) }
}
```

The ring has one producer. Hosts with several control sources must serialize them onto one `Handle`.

To read settings back on the control thread, keep a shadow `Voice` there: apply each `VoiceCmd` to it before sending, and read its getters. The `rt` module docs show the pattern and its limits.

## Parity with the C++ engine

`softcut/tests/golden.rs` replays 9 scenarios recorded from softcut-lib through softcut-py. The scenarios cover recording, overdub, varispeed in both directions, filters, rec-once, one-shot, phase quantization, engine feedback and buffer operations. Output, buffer contents and head positions match within 1.2e-7. The exception is varispeed with rate slew, at 7.6e-5: clang fuses the slew update into an FMA on arm64 and Rust does not. `make fixtures` regenerates the fixtures; see `scripts/gen_fixtures.py`.

Upstream behaviour kept for parity, besides the quirks above:

- The input filter's cutoff tracks the rate at the moment it is set, not the rate as it slews.

Deviations, all where upstream behaviour is undefined:

- Rec without play outputs 0. Upstream reads an uninitialized local.

- Rates above 64 clamp the resampler's output frame count. Upstream writes past its 64-frame buffer.

- Fade delay/window ratios above 1 are clamped. Upstream writes past its fade table.

- A non-power-of-two buffer panics. Upstream asserts in debug builds and indexes out of bounds in release builds.

## Demo

```sh
make demo
```

Four voices run over a stereo pair of 43.7 s buffers (at 48 kHz), L and R, as two linked stereo pairs: voices 1+2 and 3+4. Editing one voice of a linked pair applies to both, on opposite buffers with mirrored pan; untick "link" to control them separately. Each voice records the input channel matching its buffer. Voices 1+2 record when you press rec; voices 3+4 play back at half speed, reversed, through a lowpass. The input row picks any recording device (a mic, an interface, a virtual device such as BlackHole) and, on devices with more than two inputs, which channel pair feeds L and R. A mono device feeds both. On macOS 14.6+ and Windows it also lists output devices as "system audio" sources, which capture everything playing on that device, softcut included, so recording one while softcut plays feeds back. The input is off at startup, and its stream stays paused while off. If an input delivers nothing, or only exact silence, for 2 s, the status line says so; on macOS exact silence usually means the terminal lacks the Microphone or System Audio Recording permission. The same row picks the output device. The engine uses `Quirks::Fixed`, so recordings keep the input's polarity. "crossfade curves" sets each voice's fade shapes and ratios.

"load wav..." or dropping a file on the window loads a WAV into both buffers: stereo files split L/R, mono files fill both. Files at another rate are resampled to the device rate with a Kaiser-windowed sinc (cutoff at 95% of the lower Nyquist, about 80 dB stopband), then truncated to fit. Loading stops recording and loops every voice over the file. "save wav..." writes both buffers, over the loaded sample's length, to a stereo 32-bit float WAV. "clear loop" silences the selected voice's loop region; "reverse loop" reverses it in place. The waveform zooms to the sample on load; "fit sample" and "full buffer" switch between the two views. On the waveform, click to cut the selected voice, drag to set its loop. The engine runs at the startup output device's rate. An input device without that rate is opened at its nearest rate and resampled live with the same windowed sinc as WAV loads; the two clocks are not synchronized, so drift is absorbed by occasionally dropping or padding input frames. An output device must support the engine's rate. A failed switch leaves the previous device open.

## Development

```sh
make test   # unit, golden and doc tests, with and without the rtrb feature
make lint   # clippy -D warnings (both feature sets), rustfmt --check
```

CI (`.github/workflows/ci.yml`) runs the same checks on Linux and Windows (x86-64) and macOS (arm64), plus a rustdoc build with warnings as errors.

## License

GPL-3.0-only, as a derivative of [softcut-lib](https://github.com/monome/softcut-lib) (GPL-3.0). See `LICENSE`.
