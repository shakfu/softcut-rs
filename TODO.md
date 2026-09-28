# TODO

## Critical

- [ ] Verify the first CI run (`.github/workflows/ci.yml`). The golden fixtures were recorded on arm64; check that parity holds on x86-64 Linux and Windows, whose libms may differ from macOS on `sin`, `exp` and `tan`. The Linux package list was inferred from build scripts, not tested.

## High

- [x] Verify the demo by hand: mic input and the level meter, the file dialog, WAV drag-and-drop, and the waveform zoom. None of it has been seen or heard running.

- [ ] Publish voice settings through `rt`. `Voice` getters exist, but a control thread using `rt::Handle` cannot call them; it must keep its own copy, as the demo does.

## Medium

- [ ] `fix-upstream-quirks` feature: correct the polarity inversion (the "raised" rec fade curve computes `-sin(x)`) and the 0.1 s default fade time (upstream sets 0.01 s, then the head's init overrides it). Norns parity stays the default, so the golden tests keep passing.

- [ ] Stereo. Each voice is mono, as upstream. norns makes stereo from two voices over two buffers. The library supports several buffers; the demo uses one.

- [ ] norns buffer operations: read/copy/clear with edge fades (softcut-py's `buffer_ops.hpp`), and writing a buffer to a file, for saving loops.

- [ ] Better resampling for WAV loads than linear interpolation, for quality at mismatched sample rates.

## Low

- [ ] Fade-curve shape setters. They exist in C++ `FadeCurves`, but `softcut::Voice` never calls them.

- [ ] OSC control, and a layer mirroring the norns Lua API. Only needed to run existing norns scripts.

## Done

- [x] Buffer replacement from another thread: `Engine::replace_buffer`, `rt::Handle::load` and `Handle::returned`.

- [x] CI workflow on Linux, Windows and macOS, mirroring `make test` and `make lint`.

- [x] `Voice` getters for every setting, read from DSP state.

- [x] Demo: mic input can be switched off; the mic stream is paused unless selected.
