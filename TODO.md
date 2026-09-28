# TODO

## Critical

- [ ] Verify the first CI run (`.github/workflows/ci.yml`). The golden fixtures were recorded on arm64; check that parity holds on x86-64 Linux and Windows, whose libms may differ from macOS on `sin`, `exp` and `tan`. The Linux package list was inferred from build scripts, not tested.

## High

## Medium

- [ ] Demo: add randomization

## Low

- [ ] Demo: verify system-audio capture with the permission granted. Here the loopback stream delivers frames but exact silence, even while a sound plays, which fits a missing System Audio Recording permission.

- [ ] Publish voice settings through `rt`, only if a host with several control sources needs them. Until then a shadow `Voice` on the control thread covers it (documented in `rt`).

- [ ] OSC control, and a layer mirroring the norns Lua API. Only needed to run existing norns scripts.
