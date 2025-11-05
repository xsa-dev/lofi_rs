.PHONY: help run build fmt lint clean

help:
	@echo "Available targets:"
	@echo "  run   - Run the interactive CLI (cargo run)"
	@echo "  build - Build release (cargo build --release)"
	@echo "  fmt   - Format code (cargo fmt)"
	@echo "  lint  - Lint with clippy (cargo clippy -- -D warnings)"
	@echo "  clean - Clean target directory (cargo clean)"

run:
	cargo run

build:
	cargo build --release

fmt:
	cargo fmt

lint:
	cargo clippy -- -D warnings

clean:
	cargo clean
