//! Finite-corpus inventory guard.
//!
//! The public corpus is a fixed manifest: every crate row must exist on disk
//! with its `Cargo.toml`, a crate-local `README.md`, and a
//! `tests/public_smoke.rs` target containing the exact `public_smoke` and
//! `public_characterization` test functions. Filesystem discovery must match
//! the manifest exactly — a new public crate, a missing README, or a missing
//! proof target fails closed. Guide pages and public docs are enumerated the
//! same way.

use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("tina-runtime has a parent")
        .to_path_buf()
}

/// Every public crate row: (directory relative to repo root, crate kind).
const CRATE_ROWS: &[&str] = &[
    "examples/specimen_axum_counter",
    "examples/specimen_backpressure_chain",
    "examples/specimen_bounded_batcher",
    "examples/specimen_cancellation_chain",
    "examples/specimen_cpu_run",
    "examples/specimen_cross_shard_child_ownership",
    "examples/specimen_dynamic_worker_pool",
    "examples/specimen_graceful_drain_server",
    "examples/specimen_graceful_pool_shutdown",
    "examples/specimen_graceful_shutdown",
    "examples/specimen_grpc_counter",
    "examples/specimen_hot_key_fairness",
    "examples/specimen_http_body_streaming",
    "examples/specimen_idempotent_retry",
    "examples/specimen_local_io_codec_ipc",
    "examples/specimen_mem_run",
    "examples/specimen_mini_keyspace",
    "examples/specimen_multi_turn_request_context",
    "examples/specimen_mux_client",
    "examples/specimen_native_http",
    "examples/specimen_native_https",
    "examples/specimen_outbound_fetch",
    "examples/specimen_outbound_http",
    "examples/specimen_owned_state_leak",
    "examples/specimen_periodic_batcher",
    "examples/specimen_persistent_counter",
    "examples/specimen_pool_cancel_reclaim",
    "examples/specimen_postgres_counter",
    "examples/specimen_rate_limited_worker",
    "examples/specimen_real_io_chat",
    "examples/specimen_replay_dst",
    "examples/specimen_request_scope_fanout",
    "examples/specimen_retrying_outbound_http",
    "examples/specimen_rpc",
    "examples/specimen_scatter_gather",
    "examples/specimen_sharded_fanout_read",
    "examples/specimen_sharded_keyspace",
    "examples/specimen_sqlite_counter",
    "examples/specimen_supervised_worker",
    "examples/specimen_tcp_echo",
    "examples/specimen_tower_timeout_counter",
    "examples/specimen_tracing_demo",
    "examples/specimen_two_stage_pipeline",
    "examples/specimen_webhook_fanout",
    "examples/specimen_webhook_outbox",
    "examples/specimen_webhook_publisher",
    "examples/specimen_websocket_room",
    "examples/specimen_worker_pool",
    "examples/specimen_ws_room",
    "examples/systems/ergonomics_playground",
    "examples/systems/mini_saas_api",
    "examples/systems/perf_native",
    "examples/systems/system_api_gateway_limits",
    "examples/systems/system_bounded_object_lane",
    "examples/systems/system_cache_with_fill",
    "examples/systems/system_copied_service_path",
    "examples/systems/system_job_queue",
    "examples/systems/system_live_replay_bugbox",
    "examples/systems/system_lock_manager",
    "examples/systems/system_metrics_shipper",
    "examples/systems/system_realtime_rooms",
    "examples/systems/system_scoped_request_tree",
    "examples/systems/system_session_auth",
    "examples/systems/system_soak_http_db",
    "examples/systems/system_tenant_rate_limiter",
    "examples/systems/system_webhook_relay",
    "examples/extensions/tina-extension-capacity-surface",
    "examples/extensions/tina-extension-compile-fail",
    "examples/extensions/tina-extension-custom-codec",
    "examples/extensions/tina-extension-fake-bridge",
    "examples/extensions/tina-extension-service-policy",
];

const DOC_ROWS: &[&str] = &[
    "README.md",
    "docs/README.md",
    "docs/bridge-composition.md",
    "docs/mailbox-capacity.md",
    "docs/resource-owner-matrix.md",
    "docs/tcp-loops.md",
    "docs/tina-user-guide/README.md",
    "docs/tina-user-guide/00-agent-quickstart.md",
    "docs/tina-user-guide/01-mental-model.md",
    "docs/tina-user-guide/02-first-isolate.md",
    "docs/tina-user-guide/03-effects-and-runtime-calls.md",
    "docs/tina-user-guide/04-request-reply.md",
    "docs/tina-user-guide/05-tcp-services.md",
    "docs/tina-user-guide/06-boundedness-and-overload.md",
    "docs/tina-user-guide/07-supervision.md",
    "docs/tina-user-guide/08-simulation-and-dst.md",
    "docs/tina-user-guide/09-tokio-to-tina-porting.md",
    "docs/tina-user-guide/10-service-patterns.md",
    "docs/tina-user-guide/11-ergonomics-checklist.md",
    "docs/tina-user-guide/12-io-model.md",
    "docs/tina-user-guide/13-outcome-glossary.md",
    "docs/tina-user-guide/14-lifecycle-and-shutdown.md",
    "docs/tina-user-guide/15-service-client-worked-example.md",
    "docs/tina-user-guide/16-continuation-and-pipeline-patterns.md",
    "docs/tina-user-guide/17-pressure-report-convention.md",
    "docs/tina-user-guide/18-bridge-crates.md",
    "docs/tina-user-guide/19-tracing.md",
    "docs/tina-user-guide/20-native-websocket-server.md",
    "docs/tina-user-guide/21-compile-time-safety-rails.md",
    "docs/tina-user-guide/22-http-http2-grpc.md",
    "docs/tina-user-guide/23-core-and-batteries.md",
    "docs/tina-user-guide/24-battery-authoring.md",
    "docs/tina-user-guide/25-extension-hooks.md",
    "docs/tina-user-guide/26-async-boundary.md",
    "docs/tina-user-guide/27-which-noun-do-i-use.md",
    "docs/tina-user-guide/28-outbound-clients.md",
    "docs/tina-user-guide/29-continuation-flows.md",
    "docs/tina-user-guide/30-bridge-author-kit.md",
    "examples/README.md",
    "examples/FINDINGS.md",
    "examples/FINDINGS_HISTORY.md",
    "examples/systems/README.md",
    "examples/extensions/README.md",
    "examples/systems/perf_native/fly/README.md",
];

fn discovered_crates(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for base in ["examples", "examples/systems", "examples/extensions"] {
        let dir = root.join(base);
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.join("Cargo.toml").is_file() {
                let rel = path
                    .strip_prefix(root)
                    .expect("under root")
                    .to_string_lossy()
                    .replace('\\', "/");
                out.push(rel);
            }
        }
    }
    out.sort();
    out
}

#[test]
fn every_manifest_row_is_present_and_proved() {
    let root = repo_root();
    let mut failures = Vec::new();

    // Discovery must match the manifest exactly: no missing, no extras.
    let discovered = discovered_crates(&root);
    let expected: Vec<&str> = CRATE_ROWS.to_vec();
    for row in &expected {
        if !discovered.iter().any(|d| d == row) {
            failures.push(format!("manifest row missing on disk: {row}"));
        }
    }
    for dir in &discovered {
        if !expected.contains(&dir.as_str()) {
            failures.push(format!(
                "filesystem discovery found unlisted public crate: {dir}"
            ));
        }
    }

    for row in CRATE_ROWS {
        let dir = root.join(row);
        if !dir.join("README.md").is_file() {
            failures.push(format!("{row}: missing crate-local README.md"));
        }
        let smoke = dir.join("tests/public_smoke.rs");
        if !smoke.is_file() {
            failures.push(format!("{row}: missing tests/public_smoke.rs"));
            continue;
        }
        let text =
            fs::read_to_string(&smoke).unwrap_or_else(|e| panic!("read {}: {e}", smoke.display()));
        if !text.contains(
            "#[test]
fn public_smoke(",
        ) {
            failures.push(format!(
                "{row}: public_smoke target lacks an exact #[test] fn public_smoke"
            ));
        }
        if !text.contains(
            "#[test]
fn public_characterization(",
        ) {
            failures.push(format!(
                "{row}: public_smoke target lacks an exact #[test] fn public_characterization"
            ));
        }
    }

    for row in DOC_ROWS {
        if !root.join(row).is_file() {
            failures.push(format!("public document missing: {row}"));
        }
    }

    // Markdown discovery must match the manifest exactly: an unlisted
    // public doc fails closed the same way an unlisted crate does.
    let mut discovered_docs: Vec<String> = Vec::new();
    for base in ["docs", "docs/tina-user-guide"] {
        let dir = root.join(base);
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "md") {
                discovered_docs.push(
                    path.strip_prefix(&root)
                        .expect("under root")
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
            }
        }
    }
    for extra in &discovered_docs {
        if !DOC_ROWS.contains(&extra.as_str()) {
            failures.push(format!(
                "filesystem discovery found unlisted public document: {extra}"
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "finite-corpus inventory failed:\n{}",
        failures.join("\n")
    );
    eprintln!(
        "finite-corpus inventory: ok ({} crate rows, {} documents)",
        CRATE_ROWS.len(),
        DOC_ROWS.len()
    );
}

#[test]
fn discovery_fails_closed_on_unknown_crates() {
    // The discovery function itself is exercised against a synthetic root:
    // an extra crate dir with a Cargo.toml must surface, proving the guard
    // fails closed on corpus additions that bypass the manifest.
    let tmp = std::env::temp_dir().join(format!(
        "public corpus inventory discovery {}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(tmp.join("examples/specimen_surprise")).expect("mkdir");
    fs::write(
        tmp.join("examples/specimen_surprise/Cargo.toml"),
        "[package]\n",
    )
    .expect("write");
    let found = discovered_crates(&tmp);
    assert_eq!(found, vec!["examples/specimen_surprise".to_string()]);
    let _ = fs::remove_dir_all(&tmp);
}
