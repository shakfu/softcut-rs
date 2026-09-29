PYTHON ?= python3

.PHONY: all build test lint demo example fixtures publish publish-dry

all: build

build:
	cargo build --workspace

# softcut alone, dependency-free; then everything with the rtrb feature.
test:
	cargo test -p softcut-rs
	cargo test --workspace --all-features

lint:
	cargo clippy -p softcut-rs --all-targets -- -D warnings
	cargo clippy --workspace --all-targets --all-features -- -D warnings
	cargo fmt --all --check

demo:
	cargo run --release -p softcut-demo

example:
	cargo run --release -p softcut-rs --example render

# Regenerate golden fixtures from the C++ engine. PYTHON must have softcut-py
# installed from current source; see scripts/gen_fixtures.py.
fixtures:
	$(PYTHON) scripts/gen_fixtures.py

# Publishes softcut-rs, softcut-fx and softcut-osc in dependency order.
publish:
	cargo publish --workspace --exclude softcut-demo

# Packages and verifies as `publish` does, without uploading.
publish-dry:
	cargo publish --workspace --exclude softcut-demo --dry-run
