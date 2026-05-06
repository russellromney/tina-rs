SHELL := /bin/zsh

.PHONY: fmt check test loom miri doc clippy portable-runtime-cost verify-portable-runtime verify

fmt:
	cargo fmt --all

check:
	cargo check --workspace

test:
	cargo test --workspace

loom:
	cargo test -p tina-mailbox-spsc --features loom --test loom_spsc

miri:
	cargo +nightly miri test -p tina-mailbox-spsc --test miri_spsc

doc:
	cargo doc --workspace --no-deps

clippy:
	cargo clippy --workspace --all-targets -- -D warnings

portable-runtime-cost:
	cargo run -p tina-runtime --example portable_runtime_cost

verify-portable-runtime:
	cargo test -p tina-runtime --test portable_service
	cargo test -p tina-runtime --test local_system builder_exposes_complete_budget_manifest
	cargo test -p tina-sim --test portable_service_dst
	cargo test -p tina-tokio-bridge --test bridge_model_dst
	cargo test -p tina-tokio-bridge --test axum_bridge bridge_host_skips_cancelled_queued_request_before_user_state_mutates
	cargo run -p tina-runtime --example portable_runtime_cost | tee /tmp/tina-portable-runtime-cost.txt
	rg "cost smoke / local machine / not benchmark" /tmp/tina-portable-runtime-cost.txt
	rg "local send|live ingress|cross-shard send|isolate call|timer|TCP loopback|TLS loopback|file read/write|journal append|bridge call" /tmp/tina-portable-runtime-cost.txt

verify: fmt check test loom doc clippy
