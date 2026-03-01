.PHONY: build clean test run check fmt lint setup teardown status all

# Rust targets
build:
	cargo build

release:
	cargo build --release

clean:
	cargo clean

test:
	cargo test

check:
	cargo check

fmt:
	cargo fmt

lint:
	cargo clippy -- -D warnings

# Run (daemon auto-starts when run as root)
run:
	sudo cargo run --bin zq

# Manual daemon management
run-daemon:
	sudo cargo run --bin zq-daemon

# PF setup / teardown
setup:
	sudo cargo run --bin zq-daemon -- setup

teardown:
	sudo cargo run --bin zq-daemon -- teardown

status:
	cargo run --bin zq-daemon -- status

# Combined
all: build
