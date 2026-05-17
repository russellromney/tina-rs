SHELL := /bin/zsh

.PHONY: fmt fmt-check check test loom miri doc clippy portable-runtime-cost \
	verify verify-examples proof-fast proof-soak proof-bad-peer \
	proof-replay-regression

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

# Phase 108 proof targets. Each one is copy-pasteable into a PR check.
# `proof-fast` is the PR gate. The other three are local / nightly /
# regression slots.

# Fast PR proof: build + run the small bad-peer and replay-shape tests
# in two of the most representative system specimens. Each test owns
# its own short timeout; the whole target should finish in well under
# a minute on a developer machine.
proof-fast:
	cargo test -p tina-proof-harness
	cargo test --manifest-path examples/systems/system_realtime_rooms/Cargo.toml --test bad_peer
	cargo test --manifest-path examples/systems/system_live_replay_bugbox/Cargo.toml --test smoke

# Slow soak: the load harness against mini_saas_api with the visible
# typed capacity contract. Runs longer than the fast gate but still
# finishes in seconds; safe for a nightly cron.
proof-soak:
	cargo test --manifest-path examples/systems/mini_saas_api/Cargo.toml --test soak -- --nocapture

# Local bad-peer: the in-crate proof tests plus the realtime_rooms
# bad-peer scenarios with `--nocapture` so the typed BadPeerOutcome
# lines are visible.
proof-bad-peer:
	cargo test -p tina-proof-harness -- --nocapture
	cargo test --manifest-path examples/systems/system_realtime_rooms/Cargo.toml --test bad_peer -- --nocapture

# Replay regression: re-run the saved-seed sim cases. A mismatch fails
# loudly with the case name, scenario, history, and expected vs actual
# event count + trace hash.
proof-replay-regression:
	cargo test --manifest-path examples/systems/system_live_replay_bugbox/Cargo.toml --test smoke

verify: fmt-check check test loom doc clippy
	cargo run -p tina-runtime --example portable_runtime_cost | tee /tmp/tina-verify-cost.txt
	grep -E "cost smoke / local machine / not benchmark" /tmp/tina-verify-cost.txt
	grep -E "mailbox local push/pop|local send|live ingress|cross-shard send|isolate call|timer|TCP loopback|TLS loopback|file read/write|journal append|bridge call" /tmp/tina-verify-cost.txt
	grep -E "measured-local-smoke" /tmp/tina-verify-cost.txt
	grep -E "not-measured:" /tmp/tina-verify-cost.txt
