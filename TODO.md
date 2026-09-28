# TODO

## Critical

- [ ] Verify the first CI run (`.github/workflows/ci.yml`). The golden fixtures were recorded on arm64; check that parity holds on x86-64 Linux and Windows, whose libms may differ from macOS on `sin`, `exp` and `tan`. The Linux package list was inferred from build scripts, not tested.

## High

- [x] Verify the demo by hand: mic input and the level meter, the file dialog, WAV drag-and-drop, and the waveform zoom. None of it has been seen or heard running.

- [ ] Publish voice settings through `rt`. `Voice` getters exist, but a control thread using `rt::Handle` cannot call them; it must keep its own copy, as the demo does.

## Medium

- [ ] Better resampling for WAV loads than linear interpolation, for quality at mismatched sample rates.

## Low

- [ ] Fade-curve shape setters. They exist in C++ `FadeCurves`, but `softcut::Voice` never calls them.

- [ ] OSC control, and a layer mirroring the norns Lua API. Only needed to run existing norns scripts.

## Done

- [x] Buffer replacement from another thread: `Engine::replace_buffer`, `rt::Handle::load` and `Handle::returned`.

- [x] CI workflow on Linux, Windows and macOS, mirroring `make test` and `make lint`.

- [x] `Voice` getters for every setting, read from DSP state.

- [x] Demo: mic input can be switched off; the mic stream is paused unless selected.

- [x] Upstream quirks switchable: `Quirks::Fixed` (a constructor argument; a Cargo feature would unify across the dependency graph).

- [x] Stereo: multichannel engine input with a channel-to-voice level matrix; demo runs two linked stereo pairs over L/R buffers.

- [x] norns buffer operations (`softcut::buffer`, `ClearRegion`/`CopyRegion`, `rt::Handle::write`/`snapshot`), and WAV save in the demo.
