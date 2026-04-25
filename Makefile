SHELL := /bin/zsh

.PHONY: fmt check test loom doc clippy verify

fmt:
	cargo fmt --all

check:
	cargo check --workspace

test:
	cargo test --workspace

loom:
	cargo test -p tina-mailbox-spsc --features loom --test loom_spsc

doc:
	cargo doc --workspace --no-deps

clippy:
	cargo clippy --workspace --all-targets -- -D warnings

verify: fmt check test loom doc clippy
