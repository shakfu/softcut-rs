# TODO

## Critical

- [ ] Verify the first CI run (`.github/workflows/ci.yml`). The golden fixtures were recorded on arm64; check that parity holds on x86-64 Linux and Windows, whose libms may differ from macOS on `sin`, `exp` and `tan`. The Linux package list was inferred from build scripts, not tested.

## High

## Medium

- [ ] Investigate an effects chain in the demo, per voice or on the mix of all voices. Two sources: CLAP plugins hosted with [clack](https://github.com/prokopyl/clack) (`clack-host` 0.2 on crates.io; CLAP only, no VST3 or AU), or built-in DSP effects. Questions to settle:
  - Where it inserts. A chain on the mix can run after `Engine::process` in the demo. A chain per voice needs a hook in `Engine` between each voice's output and the mix, which the library does not expose yet.
  - Realtime safety. A plugin's `process` runs on the audio thread, but CLAP also calls back on a main thread (parameters, state, GUI). Loading, activating and removing plugins must happen off the audio thread, with a handover like the buffer swap in `rt`.
  - Built-in effects needing no dependency: delay, reverb, saturation. softcut already has a state-variable filter and a soft clipper.
  - Sample rate and latency: plugins activate at the engine's rate, and a chain adds its latency to output recording.

## Low

- [ ] Publish voice settings through `rt`, only if a host with several control sources needs them. Until then a shadow `Voice` on the control thread covers it (documented in `rt`).
