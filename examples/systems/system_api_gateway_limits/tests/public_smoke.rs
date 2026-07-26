//! Public runner proof for the API-gateway shared-capacity system.
//!
//! Public smoke drives the documented entry point
//! (`run(RunConfig::default())`, the function behind this crate's
//! `cargo test` smoke suite). Characterization pins the deterministic
//! capacity arithmetic: which budget can bind under the default config,
//! the honest full counter, and the owner-stop release invariants.

use system_api_gateway_limits::{GatewayReply, ObservedCallOutcome, RunConfig, RunReport, run};

fn assert_default_report(report: &RunReport) {
    let config = RunConfig::default();
    let total_callers = config.upload_callers + config.list_callers;
    assert_eq!(
        report.upload_admitted + report.list_admitted + report.upload_full + report.list_full,
        total_callers,
        "every caller saw a reply: {report:?}"
    );
    assert!(
        report.upload_admitted + report.list_admitted > 0,
        "at least one caller must admit: {report:?}"
    );
    let total_full = report.upload_full + report.list_full;
    assert!(
        total_full > 0,
        "shared cap 4 raced by 10 weighted callers must see Full: {report:?}"
    );
    assert_eq!(
        report.scope_full_count as usize, total_full,
        "scope full counter must match observed Full replies: {report:?}"
    );
    // Owner stop releases both budgets.
    assert_eq!(report.scope_current_at_drain, 0, "{report:?}");
    assert_eq!(report.scope_admitted, report.scope_released, "{report:?}");
    assert_eq!(report.body_current_at_drain, 0, "{report:?}");
    assert_eq!(report.body_admitted, report.body_released, "{report:?}");
    assert_eq!(
        report.refill_reply,
        Some(GatewayReply::Ok { route: "list" }),
        "capacity must refill after the caller wave: {report:?}"
    );
}

/// Documented public runner path: `run(RunConfig::default())`.
#[test]
fn public_smoke() {
    assert_default_report(&run(RunConfig::default()).expect("gateway run"));
}

/// Pins the deterministic capacity facts of the default workload.
#[test]
fn public_characterization() {
    let config = RunConfig::default();
    assert_eq!(config.shared_cap, 4);
    assert_eq!(config.body_cap, 4_096);
    assert_eq!(config.upload_weight, 2);
    assert_eq!(config.list_weight, 1);
    assert_eq!(config.upload_body, 1_024);
    assert_eq!(config.list_body, 128);
    assert_eq!(config.upload_callers, 4);
    assert_eq!(config.list_callers, 6);

    let report = run(config).expect("characterization run");
    assert_eq!(report.upload_timeout, 0, "{report:?}");
    assert_eq!(report.list_timeout, 0, "{report:?}");

    // Under the default config the in-flight budget (weight <= 4) caps
    // concurrent body bytes at 2 * 1024 = 2048 < 4096, so the body
    // budget can never bind: every Full names gateway.in_flight.
    assert_eq!(report.body_full_count, 0, "{report:?}");
    for observed in &report.caller_outcomes {
        if let ObservedCallOutcome::Replied(GatewayReply::Full { filled, .. }) = &observed.outcome {
            assert_eq!(filled, "gateway.in_flight", "{report:?}");
        }
    }

    assert!(report.scope_high_water <= config.shared_cap, "{report:?}");
    assert_eq!(
        report.scope_high_water, report.scope_high_water_at_drain,
        "high water is monotonic across shutdown: {report:?}"
    );
    assert_default_report(&report);

    // Discovery and summary lines keep their grep-friendly shape.
    assert!(
        report
            .discovery_lines
            .iter()
            .any(|line| line.starts_with("scope ") && line.contains("name=gateway.in_flight")),
        "missing in-flight discovery line: {:?}",
        report.discovery_lines
    );
    assert!(
        report
            .discovery_lines
            .iter()
            .any(|line| line.starts_with("scope ") && line.contains("name=gateway.body_bytes")),
        "missing body-bytes discovery line: {:?}",
        report.discovery_lines
    );
    assert!(
        report
            .discovery_lines
            .iter()
            .any(|line| line.starts_with("capacity ")
                && line.contains("surface=gateway.in_flight")
                && line.contains("util_bp=")),
        "missing capacity surface line: {:?}",
        report.discovery_lines
    );
    assert!(
        report
            .summary_line
            .starts_with("system=system_api_gateway_limits "),
        "summary line shape: {}",
        report.summary_line
    );
}
