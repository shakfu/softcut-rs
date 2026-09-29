# TODO

## Critical

- [ ] Verify the first CI run (`.github/workflows/ci.yml`). The golden fixtures were recorded on arm64; check that parity holds on x86-64 Linux and Windows, whose libms may differ from macOS on `sin`, `exp` and `tan`. The Linux package list was inferred from build scripts, not tested.

## High

## Medium

## Low

- [ ] Publish voice settings through `rt`, only if a host with several control sources needs them. Until then a shadow `Voice` on the control thread covers it (documented in `rt`).
