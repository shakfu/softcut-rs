# TODO

## Critical

- [ ] Verify the first CI run (`.github/workflows/ci.yml`). The golden fixtures were recorded on arm64; check that parity holds on x86-64 Linux and Windows, whose libms may differ from macOS on `sin`, `exp` and `tan`. The Linux package list was inferred from build scripts, not tested.

## High

## Medium

- [ ] softcut as a CLAP/VST3 plugin, built with [nice-plug](https://codeberg.org/RustAudio/nice-plug) (`nice-plug` 0.4, ISC; VST3 bindings MIT/Apache-2.0; `nice-plug-egui` uses egui 0.36, as the demo does; experimental per its README). A `softcut-plugin` crate would drive `Voice`/`Engine` from the host's process callback; `rt` is not needed, since the framework handles threading and parameter smoothing. Questions to settle:
  - Parameters. A plugin exposes a fixed list; softcut has 4-6 voices with about 30 settings each. Decide which are automatable and which stay in the GUI only.
  - Session state. Two 43.7 s buffers (2^21 frames each, as in the demo) are ~17 MB of f32 to store with the host's session; the library's default of 2^24 frames is 8 times that. Options: store them compressed (nice-plug has a `zstd` feature), store only the used loop regions, or reference files on disk.
  - GUI. Reuse the demo's egui waveform lanes and voice controls through `nice-plug-egui`.
  - Input routing. A host's stereo input maps onto the engine's two input channels; sidechain input is optional.

## Low

- [ ] Demo: host CLAP plugins in the effects chains with [clack](https://github.com/prokopyl/clack) (`clack-host` 0.2; CLAP only). Built-in effects exist (`softcut-fx`). A plugin's `process` runs on the audio thread, but CLAP also calls back on a main thread (parameters, state, GUI); loading and removing plugins needs a handover off the audio thread, like the buffer swap in `rt`. (nice-plug was ruled out: it builds plugins but does not host them.)

- [ ] Publish voice settings through `rt`, only if a host with several control sources needs them. Until then a shadow `Voice` on the control thread covers it (documented in `rt`).
