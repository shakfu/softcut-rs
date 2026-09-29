# Changelog

All notable changes to this project. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow [Semantic Versioning](https://semver.org/).

## [Unreleased]

First version: a Rust port of [softcut-lib](https://github.com/monome/softcut-lib), OSC control over its reference protocol, and an egui demo.

### Added

#### softcut

- `Voice`: softcut-lib's voice, with crossfaded, resampling read/write heads and pre/post filters. It borrows its buffer on each `process_block` call rather than storing a pointer, so voices share a buffer without `unsafe`. Getters read DSP state, so they report what the voice does rather than the last value set.
- `Engine`: a multi-voice host that owns the buffers, with level, pan, input gain, a feedback matrix, and multichannel input routed by a channel-to-voice level matrix.
- `VoiceCmd` and `EngineCmd`: `Copy` commands for queueing changes to the audio thread.
- `Engine::process_with` and `rt::Processor::process_with`: a per-voice insert on each voice's output before pan and mix. It is a closure rather than an effect the engine owns, so nothing needs handing to or freeing on the audio thread. Voice-to-voice feedback carries the processed signal.
- `Quirks`: `Upstream` (the default) reproduces softcut-lib; `Fixed` corrects its polarity inversion, its 0.1 s fade after reset, and its raised pre-curve check. It is a constructor argument rather than a Cargo feature, because a feature enabled anywhere in a dependency graph would change every crate's output.
- Fade-curve shapes and ratios on `Voice`. softcut-lib implements them but its `Voice` does not expose them.
- `Voice::heads` (both crossfading heads' position, fade and gain) and `rec_fade_value`/`pre_fade_value`, for visualizing crossfades; `rt::Handle::heads` publishes the heads.
- `buffer`: norns buffer operations (write, clear, copy) on slices, and `EngineCmd::ClearRegion` and `CopyRegion` to run them on the audio thread. Copies within one buffer need no temporary; a reversed copy between partly overlapping regions is refused rather than allocating.
- `rt` (feature `rtrb`): a `Handle` for the control thread and a `Processor` for the audio thread. Commands, buffer loads, writes and snapshots share one ring, so they apply in the order sent. Every buffer sent comes back to the control thread, so the audio thread never frees memory.
- Golden tests against softcut-lib, run through softcut-py, under both `Quirks` modes. They check the `rec`, `rec_once` and `fade_time` that softcut-lib changes itself, as well as audio and position. The README's "Parity with the C++ engine" section gives tolerances and deviations. A separate test checks that faded loop wraps, in playback and overdub, add no click.

#### softcut-fx

- A `Chain` of saturation, bitcrusher, chorus, delay and reverb (Freeverb), mono or stereo, each switchable, driven by `FxCmd`. It allocates only when created. The delay glides between times in f64: in f32 the glide stalls short of its target, by about 9 frames at 2 s, leaving a fractional delay that dulls the echo.

#### softcut-osc

- OSC control over the protocol of softcut-lib's reference client, `softcut_jack_osc`, which norns uses to drive its audio engine. Indices are 0-based. Covers every `/set/param/cut/*` setting, level, pan, input and feedback routing, `/softcut/buffer/*` reads, writes and clears, `/softcut/reset`, and the phase poll.
- `parse`: one OSC message to `Action`s, mostly `EngineCmd`s. File reads and writes, reset and the phase poll are left to the host, which owns files and threads.
- `Server`: receives UDP on its own thread and forwards actions over a bounded channel, so the host applies them through its one control path, such as an `rt::Handle`. It listens on loopback by default, because the protocol writes files at paths the sender names. A full channel drops actions and counts them.
- `PhasePoll`: reports each voice's quantized position as `/poll/softcut/phase i f` when it changes.
- Arguments are type-checked, ints and floats coerced as the reference's liblo does, and non-finite numbers rejected before they reach the engine.
- `level_slew_time`, `pan_slew_time`, `/set/enabled/cut`, the VU poll and `/quit` are accepted and ignored: the engine has no counterpart, and a network message does not quit the host.

#### softcut-demo

- Four voices as two linked stereo pairs over L and R buffers, with waveform lanes showing loops and playheads, per-voice controls and a feedback matrix.
- WAV load with windowed-sinc resampling, saving the selected voice's loop region as a 32-bit float WAV (stereo for a linked pair), and loop clear and reverse.
- Output recording: the mix of all voices to a stereo 32-bit float WAV, written by a separate thread so the audio thread does no file I/O.
- Effects: a mono chain on each voice and a stereo chain on the mix.
- Crossfade visualization: fade curves on each loop band, both heads drawn at their gain, and a plot of the rec and pre curves.
- OSC control, with the controls following OSC changes.
- Feedback routing presets (off, cascade, cross, exchange, swap sides, ring) alongside the manual matrix. Each gives a voice at most one source and is symmetric across L and R, so linked pairs stay stereo. Amount is capped at 0.4 so feedback decays at the preset `pre_level` and `rec_level`. Picking a preset keeps a hand-edited matrix, which "manual" restores. Randomizing all voices can draw one, opt-in, since feedback writes into the buffers.
- Randomization of rate, loop region, pan and level, and filter, per voice or for all voices, once or on a timer. Rates are drawn from octaves and fifths so results stay in tune.
- Input and output device selection. Devices at other rates, input or output, are resampled live, and resampled input is steered against clock drift rather than dropping or padding frames. Output devices appear as system-audio sources on macOS 14.6+ and Windows. The status line reports an input that is exactly silent, which on macOS usually means a missing permission.

[Unreleased]: https://github.com/shakfu/softcut-rs/commits/main
