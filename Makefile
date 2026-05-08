SHELL := /bin/zsh

.PHONY: fmt fmt-check check test loom miri doc clippy portable-runtime-cost \
	verify verify-examples

fmt:
	cargo fmt --all

fmt-check:
	cargo fmt --all --check

check:
	cargo check --workspace

test:
	cargo test --workspace

loom:
	cargo test -p tina-mailbox-spsc --features loom --test loom_spsc

miri:
	cargo +nightly miri test -p tina-mailbox-spsc --test miri_spsc

doc:
	RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps

clippy:
	cargo clippy --workspace --all-targets -- -D warnings

# Walks examples/*/Cargo.toml (each is its own cargo workspace, excluded
# from the main one). Builds + tests each so a workspace-only change
# can't silently break a downstream specimen. Stops on first failure.
verify-examples:
	@set -e; \
	for manifest in examples/*/Cargo.toml; do \
		echo "==> $$manifest"; \
		cargo test --manifest-path "$$manifest"; \
		cargo clippy --manifest-path "$$manifest" --all-targets -- -D warnings; \
	done

portable-runtime-cost:
	cargo run -p tina-runtime --example portable_runtime_cost

verify: fmt-check check test loom doc clippy
	cargo run -p tina-runtime --example portable_runtime_cost | tee /tmp/tina-verify-cost.txt
	grep -E "cost smoke / local machine / not benchmark" /tmp/tina-verify-cost.txt
	grep -E "mailbox local push/pop|local send|live ingress|cross-shard send|isolate call|timer|TCP loopback|TLS loopback|file read/write|journal append|bridge call" /tmp/tina-verify-cost.txt
	grep -E "measured-local-smoke" /tmp/tina-verify-cost.txt
	grep -E "not-measured:" /tmp/tina-verify-cost.txt
