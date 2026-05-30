SHELL := /bin/zsh
EXAMPLES_TARGET_DIR ?= $(CURDIR)/target/verify-examples

.PHONY: fmt fmt-check check test loom miri doc clippy portable-runtime-cost perf \
	perf-compare verify verify-examples proof-fast proof-soak proof-bad-peer \
	proof-replay-regression race-surface-guard rail-inventory-guard

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
	cargo test -p tina-runtime --features loom --test loom_shared_scope

# Race-surface guard: fail if a new shared-memory synchronization primitive
# (UnsafeCell / unsafe impl Send|Sync / atomic) appears in core-crate code
# outside the reviewed allowlist (.intent/race-surface-allowlist.txt).
# Surrogate proof — it catches additions; the loom models prove the existing
# structures.
race-surface-guard:
	./scripts/race_surface_guard.sh

# Rail-inventory guard: fail if a runtime-owned rail adds a worker thread,
# blocking std socket, or blocking std::fs work outside the reviewed inventory
# (.intent/runtime-rail-inventory.txt). Enforces the Phase 140 rule that every
# rail rides the Betelgeuse substrate or is an inventoried blocking/fallback
# lane with a written reason.
rail-inventory-guard:
	./scripts/rail_inventory_guard.sh

miri:
	cargo +nightly miri test -p tina-mailbox-spsc --test miri_spsc

doc:
	RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps

clippy:
	cargo clippy --workspace --all-targets -- -D warnings

# Walks examples/*/Cargo.toml and the extension smoke crates under
# examples/extensions/*/Cargo.toml (each is its own cargo workspace,
# excluded from the main one). Builds + tests each so a workspace-only
# change can't silently break a downstream specimen or extension crate.
# Stops on first failure.
verify-examples:
	@set -e; \
	for manifest in examples/*/Cargo.toml examples/extensions/*/Cargo.toml; do \
		echo "==> $$manifest"; \
		CARGO_TARGET_DIR="$(EXAMPLES_TARGET_DIR)" cargo test --manifest-path "$$manifest"; \
		CARGO_TARGET_DIR="$(EXAMPLES_TARGET_DIR)" cargo clippy --manifest-path "$$manifest" --all-targets -- -D warnings; \
	done

portable-runtime-cost:
	cargo run -p tina-runtime --example portable_runtime_cost

# Local performance evidence. Release mode, local machine. Prints timing plus
# boundedness truth: pressure, capacity surfaces, leak/shutdown facts, and
# native Tina-vs-bounded-Tokio comparison rows where semantics are explicit.
perf:
	cargo run --release -p tina-runtime --example portable_runtime_cost
	cargo test --release -p tina-proof-harness perf_report -- --nocapture
	cargo test --release --manifest-path examples/systems/perf_native/Cargo.toml --test perf -- --nocapture
	cargo test --release --manifest-path examples/systems/perf_native/Cargo.toml --test hotpath -- --nocapture
	cargo test --release --manifest-path examples/systems/mini_saas_api/Cargo.toml --test perf -- --nocapture

perf-compare:
	cargo test --release --manifest-path examples/systems/perf_native/Cargo.toml --test perf -- --nocapture

# Record current `make perf` rows to perf_history.jsonl, append-only.
# Use before merging a perf-relevant change so future runs can diff against it.
perf-record:
	./scripts/perf_record.sh

# Run `make perf` and compare each row's tina_p50_ns against the median of the
# most recent runs in perf_history.jsonl. Exits non-zero on regression.
# Tune via PERF_CHECK_WINDOW (default 5) and PERF_CHECK_THRESHOLD (default 25).
perf-check:
	./scripts/perf_check.sh

# Phase 108 proof targets. Each one is copy-pasteable into a PR check.
# `proof-fast` is the PR gate. The other three are local / nightly /
# regression slots.

# Fast PR proof: build + run the small bad-peer and replay-shape tests
# in two of the most representative system specimens, plus the bounded
# protocol-chaos corpus (WebSocket compliance cases, HTTP/2 + gRPC
# probes, byte replay). `cargo test -p tina-proof-harness` runs the
# in-crate corpus and the `protocol_regression` suite. Each test owns
# its own short timeout; the whole target should finish in well under
# a minute on a developer machine.
proof-fast:
	cargo test -p tina-proof-harness
	cargo test --manifest-path examples/systems/system_realtime_rooms/Cargo.toml --test bad_peer
	cargo test --manifest-path examples/systems/system_live_replay_bugbox/Cargo.toml --test smoke

# Slow soak: the load harness against mini_saas_api with the visible
# typed capacity contract, plus the protocol-chaos corpus repeated at a
# higher count via TINA_PROTOCOL_SOAK_ITERS. Same semantics as the fast
# gate, just more reps; finishes in seconds and is safe for a nightly cron.
proof-soak:
	cargo test --manifest-path examples/systems/mini_saas_api/Cargo.toml --test soak -- --nocapture
	TINA_PROTOCOL_SOAK_ITERS=500 cargo test -p tina-proof-harness --test protocol_regression protocol_chaos_soak -- --nocapture

# Local bad-peer: the in-crate proof tests plus the realtime_rooms
# bad-peer scenarios with `--nocapture` so the typed BadPeerOutcome and
# ProtocolChaosReport lines are visible.
proof-bad-peer:
	cargo test -p tina-proof-harness -- --nocapture
	cargo test -p tina-proof-harness --test protocol_regression print_typed_protocol_chaos_reports -- --nocapture
	cargo test --manifest-path examples/systems/system_realtime_rooms/Cargo.toml --test bad_peer -- --nocapture

# Replay regression: re-run the saved-seed sim cases. A mismatch fails
# loudly with the case name, scenario, history, and expected vs actual
# event count + trace hash.
proof-replay-regression:
	cargo test --manifest-path examples/systems/system_live_replay_bugbox/Cargo.toml --test smoke

verify: fmt-check check test loom race-surface-guard rail-inventory-guard doc clippy
	cargo run -p tina-runtime --example portable_runtime_cost | tee /tmp/tina-verify-cost.txt
	grep -E "cost rows / local_machine comparison_baseline=none" /tmp/tina-verify-cost.txt
	grep -E "mailbox local push/pop|local send|live ingress|cross-shard send|isolate call|timer|TCP loopback|TLS loopback|file read/write|journal append|bridge call" /tmp/tina-verify-cost.txt
	grep -E "measured-local-cost" /tmp/tina-verify-cost.txt
	grep -E "not-measured:" /tmp/tina-verify-cost.txt
