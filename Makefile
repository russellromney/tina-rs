SHELL := /bin/zsh
EXAMPLES_TARGET_DIR ?= $(CURDIR)/target/verify-examples

# Local sccache (optional, mirrors CI's compile cache): install sccache
# (`cargo install sccache --locked` or `brew install sccache`), then
#   export RUSTC_WRAPPER=sccache CARGO_INCREMENTAL=0
# before running any `make` target. CARGO_INCREMENTAL=0 is required —
# sccache does not cache incremental artifacts, so leaving incremental on
# just adds overhead with no cache benefit. `sccache --show-stats` reports
# hit rate.

.PHONY: fmt fmt-check check test loom miri doc clippy portable-runtime-cost perf \
	perf-compare verify verify-static verify-guards verify-examples verify-packaging proof-fast proof-soak \
	proof-long-soak proof-bad-peer proof-replay-regression race-surface-guard rail-inventory-guard \
	examples-startup-api-guard

fmt:
	cargo fmt --all

fmt-check:
	cargo fmt --all --check

# Not a `verify` prerequisite: `cargo clippy --all-targets` already type-checks
# everything `cargo check` does, so running both back-to-back double-compiles
# the whole workspace for no extra coverage. Kept as a standalone target for a
# quick local check-only loop (faster than clippy, no lints).
check:
	cargo check --workspace --locked

# nextest runs everything except doctests (it can't execute them), so the
# doctest pass below is required for parity with plain `cargo test`. nextest's
# per-test-binary isolation also fixes the aggregate-run flakiness the
# trybuild compile-fail suites (admission_compile_fail, flow_macro_compile_fail)
# see under a single `cargo test` process.
test:
	cargo nextest run --workspace --locked
	cargo test --workspace --doc --locked

loom:
	cargo test --locked -p tina-mailbox-spsc --features loom --test loom_spsc
	cargo test --locked -p tina-runtime --features loom --test loom_shared_scope

# Race-surface guard: fail if a new shared-memory synchronization primitive
# (UnsafeCell / unsafe impl Send|Sync / atomic) appears in core-crate code
# outside the reviewed allowlist (.intent/race-surface-allowlist.txt).
# Surrogate proof — it catches additions; the loom models prove the existing
# structures.
race-surface-guard:
	./scripts/race_surface_guard.sh

# Rail-inventory guard: fail if a runtime-owned rail adds a worker thread,
# blocking std socket, or blocking std::fs work outside the reviewed inventory
# (.intent/runtime-rail-inventory.txt). Enforces the runtime rail invariant that every
# rail rides the Betelgeuse substrate or is an inventoried blocking/fallback
# lane with a written reason.
rail-inventory-guard:
	./scripts/rail_inventory_guard.sh

examples-startup-api-guard:
	./scripts/examples_startup_api_guard.sh --self-test
	./scripts/examples_startup_api_guard.sh

miri:
	cargo +nightly miri test --locked -p tina-mailbox-spsc --test miri_spsc

doc:
	RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked

clippy:
	cargo clippy --locked --workspace --all-targets -- -D warnings

# Walks examples/*/Cargo.toml, examples/systems/*/Cargo.toml, and the
# extension smoke crates under examples/extensions/*/Cargo.toml (each is its
# own cargo workspace, excluded from the main one). Builds + tests each so a
# workspace-only change can't silently break a downstream specimen, systems
# example, or extension crate. These example workspaces intentionally remain
# unlocked until each one has an explicit lockfile policy. Stops on first
# failure. This is the one authoritative command that validates everything
# promoted as a user starting point; `examples/systems/*` used to be excluded
# by the glob (one level too shallow), so the systems examples — including
# `system_copied_service_path`, the flagship copied-path skeleton — silently
# sat outside this sweep. `system_copied_service_path` and `mini_saas_api`
# also run in the `systems-examples` CI job (`.github/workflows/verify.yml`)
# on every PR; the rest of `examples/systems/*` is local-sweep-only for now
# (see that job's comment for why).
verify-examples:
	@set -e; \
	for manifest in examples/*/Cargo.toml examples/systems/*/Cargo.toml examples/extensions/*/Cargo.toml; do \
		echo "==> $$manifest"; \
		CARGO_TARGET_DIR="$(EXAMPLES_TARGET_DIR)" cargo test --manifest-path "$$manifest"; \
		CARGO_TARGET_DIR="$(EXAMPLES_TARGET_DIR)" cargo clippy --manifest-path "$$manifest" --all-targets -- -D warnings; \
	done

portable-runtime-cost:
	cargo run --locked -p tina-runtime --example portable_runtime_cost

# Local performance evidence. Release mode, local machine. Prints timing plus
# boundedness truth: pressure, capacity surfaces, leak/shutdown facts, and
# native Tina-vs-bounded-Tokio comparison rows where semantics are explicit.
perf:
	cargo run --locked --release -p tina-runtime --example portable_runtime_cost
	cargo test --locked --release -p tina-proof-harness perf_report -- --nocapture
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

# Proof targets. Each one is copy-pasteable into a PR check.
# `proof-fast` is the PR gate. The other three are local / nightly /
# regression slots.

# Fast PR proof: build + run the small bad-peer and replay-shape tests
# in two of the most representative system specimens, plus the bounded
# protocol-chaos corpus (WebSocket compliance cases, HTTP/2 + gRPC
# probes, byte replay). `cargo test -p tina-proof-harness` runs the
# in-crate corpus and the `protocol_regression` suite. Each test owns
# its own short timeout; the whole target should finish in well under
# a minute on a developer machine. In CI: `tina-proof-harness` rides the
# workspace `test` job (it's a workspace member); the three standalone
# example suites run in the `systems-examples` job in verify.yml.
proof-fast:
	cargo test --locked -p tina-proof-harness
	cargo test --manifest-path examples/systems/system_realtime_rooms/Cargo.toml --test bad_peer
	cargo test --manifest-path examples/systems/system_live_replay_bugbox/Cargo.toml --test smoke
	cargo test --manifest-path examples/systems/system_scoped_request_tree/Cargo.toml --test smoke

# Slow soak: the load harness against mini_saas_api with the visible
# typed capacity contract, plus the protocol-chaos corpus repeated at a
# higher count via TINA_PROTOCOL_SOAK_ITERS. Same semantics as the fast
# gate, just more reps; finishes in seconds and is safe for a nightly cron.
proof-soak:
	cargo test --manifest-path examples/systems/mini_saas_api/Cargo.toml --test soak -- --nocapture
	TINA_PROTOCOL_SOAK_ITERS=500 cargo test --locked -p tina-proof-harness --test protocol_regression protocol_chaos_soak -- --nocapture

# Opt-in long soak. Not a normal PR gate. Default is 10 minutes; set
# TINA_LONG_SOAK_SECONDS=3600 for the one-hour run.
proof-long-soak:
	cargo test --manifest-path examples/systems/mini_saas_api/Cargo.toml --test long_soak -- --ignored --nocapture

# Local bad-peer: the in-crate proof tests plus the realtime_rooms
# bad-peer scenarios with `--nocapture` so the typed BadPeerOutcome and
# ProtocolChaosReport lines are visible.
proof-bad-peer:
	cargo test --locked -p tina-proof-harness -- --nocapture
	cargo test --locked -p tina-proof-harness --test protocol_regression print_typed_protocol_chaos_reports -- --nocapture
	cargo test --manifest-path examples/systems/system_realtime_rooms/Cargo.toml --test bad_peer -- --nocapture

# Replay regression: re-run the saved-seed sim cases. A mismatch fails
# loudly with the case name, scenario, history, and expected vs actual
# event count + trace hash.
proof-replay-regression:
	cargo test --manifest-path examples/systems/system_live_replay_bugbox/Cargo.toml --test smoke

# Split of `verify` into independent groups, so CI can run them as concurrent
# jobs (wall-clock = slowest group, not the sum). Each group is also a valid
# standalone local target.
#
# `clippy` is intentionally NOT folded in here: it must run per-platform
# (clippy only lints code it compiles for the target, and this workspace has
# macOS-only cfg blocks), so CI matrixes the standalone `clippy` target over
# both OSes. fmt-check and doc are platform-independent, so this group runs
# ubuntu-only.
verify-static: fmt-check doc

verify-guards: loom race-surface-guard rail-inventory-guard examples-startup-api-guard
	cargo run --locked -p tina-runtime --example portable_runtime_cost | tee /tmp/tina-verify-cost.txt
	grep -E "cost rows / local_machine comparison_baseline=none" /tmp/tina-verify-cost.txt
	grep -E "mailbox local push/pop|local send|live ingress|cross-shard send|isolate call|timer|TCP loopback|TLS loopback|file read/write|journal append|bridge call" /tmp/tina-verify-cost.txt
	grep -E "measured-local-cost" /tmp/tina-verify-cost.txt
	grep -E "not-measured:" /tmp/tina-verify-cost.txt

# Full sequential gate, for a single local command. CI runs the groups above
# (plus a per-platform `clippy`) as parallel jobs instead of this target.
verify: verify-static clippy test verify-guards

# Packaging-readiness smoke check for the crates.io 0.1.0 prep. Two parts:
#
# 1. `cargo package --no-verify` on the crates that can package today. This
#    proves the manifest packages AND fails outright on a versionless
#    `path`-only internal dependency ("all dependencies must have a version
#    requirement specified when packaging"). --no-verify skips the build
#    (already covered by `test`/`clippy`). Only these three run: any crate
#    that depends on tina-runtime (directly or transitively) pulls in the
#    vendored ../vendor-betelgeuse path dependency, which has no published
#    version to fall back to, so `cargo package` on it fails regardless of
#    manifest correctness (the open Betelgeuse question). And even among
#    publishable crates, one that depends on another workspace crate can only
#    pass once that dependency is live on crates.io -- inherent to first-time
#    interdependent releases. tina-macros, tina-rpc-macros, and tina-codec
#    have zero internal dependencies, so they package with no prerequisite.
#
# 2. `packaging_metadata_guard.sh` backstops the two things `cargo package`
#    does NOT catch: a missing description/license/repository (cargo only
#    WARNS, exit 0) and a versionless path dep in a crate that can't package
#    at all (never reached by part 1). It asserts both across every workspace
#    crate. Without it, a crate could lose its publish metadata and this job
#    would stay green.
verify-packaging:
	cargo package --allow-dirty --no-verify -p tina-macros
	cargo package --allow-dirty --no-verify -p tina-rpc-macros
	cargo package --allow-dirty --no-verify -p tina-codec
	bash scripts/packaging_metadata_guard.sh
