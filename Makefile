PYTHON ?= python3

.PHONY: all build test lint demo example fixtures

all: build

build:
	cargo build --workspace

# softcut alone, dependency-free; then everything with the rtrb feature.
test:
	cargo test -p softcut
	cargo test --workspace --all-features

lint:
	cargo clippy -p softcut --all-targets -- -D warnings
	cargo clippy --workspace --all-targets --all-features -- -D warnings
	cargo fmt --all --check

demo:
	cargo run --release -p softcut-demo

example:
	cargo run --release -p softcut --example render

# Regenerate golden fixtures from the C++ engine. PYTHON must have softcut-py
# installed from current source; see scripts/gen_fixtures.py.
fixtures:
	$(PYTHON) scripts/gen_fixtures.py
