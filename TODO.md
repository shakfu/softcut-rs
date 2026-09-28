# TODO

## Critical

- [ ] Verify the first CI run (`.github/workflows/ci.yml`). The golden fixtures were recorded on arm64; check that parity holds on x86-64 Linux and Windows, whose libms may differ from macOS on `sin`, `exp` and `tan`. The Linux package list was inferred from build scripts, not tested.

## High

- [x] Verify the demo by hand: mic input and the level meter, the file dialog, WAV drag-and-drop, and the waveform zoom. None of it has been seen or heard running.

## Medium

## Low

- [ ] Demo: resample input from devices that lack the output's sample rate (e.g. a 48 kHz-only device beside 44.1 kHz speakers), instead of refusing them.

- [ ] Demo: system-audio capture. cpal 0.18 opens an output device as a loopback input on macOS 14.6+, but a probe here built the stream and received no callbacks, likely because an unbundled binary cannot request the System Audio Recording permission.

- [ ] Demo: output device selection. The output stream's callback owns the engine, so switching needs a handover.

- [ ] Publish voice settings through `rt`, only if a host with several control sources needs them. Until then a shadow `Voice` on the control thread covers it (documented in `rt`).

- [ ] OSC control, and a layer mirroring the norns Lua API. Only needed to run existing norns scripts.

## Done

- [x] Buffer replacement from another thread: `Engine::replace_buffer`, `rt::Handle::load` and `Handle::returned`.

- [x] CI workflow on Linux, Windows and macOS, mirroring `make test` and `make lint`.

- [x] `Voice` getters for every setting, read from DSP state.

- [x] Demo: mic input can be switched off; the mic stream is paused unless selected.

- [x] Upstream quirks switchable: `Quirks::Fixed` (a constructor argument; a Cargo feature would unify across the dependency graph).

- [x] Stereo: multichannel engine input with a channel-to-voice level matrix; demo runs two linked stereo pairs over L/R buffers.

- [x] norns buffer operations (`softcut::buffer`, `ClearRegion`/`CopyRegion`, `rt::Handle::write`/`snapshot`), and WAV save in the demo.

- [x] Windowed-sinc resampling for WAV loads in the demo.

- [x] Fade-curve shapes and ratios on `Voice`, with the upstream raised-pre bug under `Quirks::Upstream`.

- [x] Demo: input device and channel-pair selection.
