# Changelog

All notable changes to this project. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow [Semantic Versioning](https://semver.org/).

## [Unreleased]

First version: a Rust port of [softcut-lib](https://github.com/monome/softcut-lib), and an egui demo.

### Added

#### softcut

- `Voice`: softcut-lib's voice, with crossfaded, resampling read/write heads and pre/post filters. It borrows its buffer on each `process_block` call rather than storing a pointer, so voices share a buffer without `unsafe`. Getters read DSP state, so they report what the voice does rather than the last value set.
- `Engine`: a multi-voice host that owns the buffers, with level, pan, input gain, a feedback matrix, and multichannel input routed by a channel-to-voice level matrix.
- `VoiceCmd` and `EngineCmd`: `Copy` commands for queueing changes to the audio thread.
- `Quirks`: `Upstream` (the default) reproduces softcut-lib; `Fixed` corrects its polarity inversion, its 0.1 s fade after reset, and its raised pre-curve check. It is a constructor argument rather than a Cargo feature, because a feature enabled anywhere in a dependency graph would change every crate's output.
- Fade-curve shapes and ratios on `Voice`. softcut-lib implements them but its `Voice` does not expose them.
- `buffer`: norns buffer operations (write, clear, copy) on slices, and `EngineCmd::ClearRegion` and `CopyRegion` to run them on the audio thread. Copies within one buffer need no temporary; a reversed copy between partly overlapping regions is refused rather than allocating.
- `rt` (feature `rtrb`): a `Handle` for the control thread and a `Processor` for the audio thread. Commands, buffer loads, writes and snapshots share one ring, so they apply in the order sent. Every buffer sent comes back to the control thread, so the audio thread never frees memory.
- Golden tests against softcut-lib, run through softcut-py. The README's "Parity with the C++ engine" section gives tolerances and deviations. A separate test checks that faded loop wraps, in playback and overdub, add no click.

#### softcut-demo

- Four voices as two linked stereo pairs over L and R buffers, with waveform lanes showing loops and playheads, per-voice controls and a feedback matrix.
- WAV load with windowed-sinc resampling, stereo 32-bit float WAV save, and loop clear and reverse.
- Input and output device selection. Devices at other rates, input or output, are resampled live, and resampled input is steered against clock drift rather than dropping or padding frames. Output devices appear as system-audio sources on macOS 14.6+ and Windows. The status line reports an input that is exactly silent, which on macOS usually means a missing permission.

[Unreleased]: https://github.com/shakfu/softcut-rs/commits/main
