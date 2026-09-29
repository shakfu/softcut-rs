# TODO

## Critical

- [x] Verify the first CI run (`.github/workflows/ci.yml`). The golden fixtures were recorded on arm64; check that parity holds on x86-64 Linux and Windows, whose libms may differ from macOS on `sin`, `exp` and `tan`. The Linux package list was inferred from build scripts, not tested. Done: lint and tests pass on macOS, Ubuntu and Windows ([runs](https://github.com/shakfu/softcut-rs/actions)).

## High

- [x] Golden fixtures do not check `rec`, `rec_once`, `play` or `fade_time`. Done: `.state` records all four, so the `rec_once` fixture now checks when the pass ends, to the block.

- [x] `Quirks::Fixed` has unit tests but no golden fixtures. Done: every voice and engine scenario also runs under `Fixed`, against `fixtures/fixed/`. `Upstream` stays the default.

- [ ] The golden fixtures, including `fixtures/fixed/`, were generated from softcut-py's uncommitted working tree (quirks switch and DSP read-backs). Once softcut-py commits that work, regenerate with `make fixtures` and confirm the files are unchanged.

- [ ] No golden scenario covers the pre-curve quirk (`calcPreFade` testing `recShape`): it applies only with a `Raised` pre shape over a non-`Raised` rec shape, and softcut-py cannot set fade shapes yet (its TODO, Medium). Add a scenario once it can.

## Medium

- [ ] softcut as a CLAP/VST3 plugin, built with [nice-plug](https://codeberg.org/RustAudio/nice-plug) (`nice-plug` 0.4, ISC; VST3 bindings MIT/Apache-2.0; `nice-plug-egui` uses egui 0.36, as the demo does; experimental per its README). A `softcut-plugin` crate would drive `Voice`/`Engine` from the host's process callback; `rt` is not needed, since the framework handles threading and parameter smoothing. Questions to settle:
  - Parameters. A plugin exposes a fixed list; softcut has 4-6 voices with about 30 settings each. Decide which are automatable and which stay in the GUI only.
  - Session state. Two 43.7 s buffers (2^21 frames each, as in the demo) are ~17 MB of f32 to store with the host's session; the library's default of 2^24 frames is 8 times that. Options: store them compressed (nice-plug has a `zstd` feature), store only the used loop regions, or reference files on disk.
  - GUI. Reuse the demo's egui waveform lanes and voice controls through `nice-plug-egui`.
  - Input routing. A host's stereo input maps onto the engine's two input channels; sidechain input is optional.

## Low

- [ ] Demo: host CLAP plugins in the effects chains with [clack](https://github.com/prokopyl/clack) (`clack-host` 0.2; CLAP only). Built-in effects exist (`softcut-fx`). A plugin's `process` runs on the audio thread, but CLAP also calls back on a main thread (parameters, state, GUI); loading and removing plugins needs a handover off the audio thread, like the buffer swap in `rt`. (nice-plug was ruled out: it builds plugins but does not host them.)

- [ ] Audio device and WAV I/O live only in `softcut-demo`, so each host re-implements `softcut-osc`'s `ReadMono`/`WriteMono` actions and device setup. softcut-py puts both in the library. Move them into a crate only if a second host needs them.

- [ ] Publish voice settings through `rt`, only if a host with several control sources needs them. Until then a shadow `Voice` on the control thread covers it (documented in `rt`).
