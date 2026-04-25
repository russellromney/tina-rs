SHELL := /bin/zsh

.PHONY: fmt check test doc clippy verify

fmt:
	cargo fmt --all

check:
	cargo check --workspace

test:
	cargo test --workspace

doc:
	cargo doc --workspace --no-deps

clippy:
	cargo clippy --workspace --all-targets -- -D warnings

verify: fmt check test doc clippy
