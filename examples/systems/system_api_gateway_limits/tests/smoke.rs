use system_api_gateway_limits::{RunConfig, run};

#[test]
fn shared_scope_fills_and_releases_across_routes() {
    let config = RunConfig {
        upload_callers: 4,
        list_callers: 6,
        ..RunConfig::default()
    };
    let report = run(config).expect("gateway run");

    // Cap is 4; upload weight 2 means at most two uploads in flight.
    // Six list callers (weight 1) plus four upload callers race
    // against shared_cap=4. Total admitted weight peaks at 4.
    assert_eq!(
        report.upload_admitted + report.list_admitted + report.upload_full + report.list_full,
        config.upload_callers + config.list_callers,
        "every caller saw a reply",
    );

    assert!(
        report.upload_admitted > 0,
        "at least one upload should admit, got report={report:?}",
    );
    assert!(
        report.list_admitted > 0,
        "at least one list should admit, got report={report:?}",
    );
    let total_full = report.upload_full + report.list_full;
    assert!(
        total_full > 0,
        "shared cap is 4 but {} weighted callers raced — expected some Full, got report={report:?}",
        config.upload_callers * config.upload_weight + config.list_callers * config.list_weight,
    );
    assert_eq!(
        report.scope_full_count as usize, total_full,
        "scope.full_count should match observed Full replies",
    );

    // The scope drained: nothing held after the run.
    assert_eq!(
        report.scope_current_at_drain, 0,
        "owner stop must release every held charge — report={report:?}",
    );
    assert_eq!(
        report.scope_admitted, report.scope_released,
        "every admitted weight must be released — report={report:?}",
    );

    // Discovery lines are grep-friendly.
    assert!(
        report
            .discovery_lines
            .iter()
            .any(|l| l.starts_with("scope ") && l.contains("name=gateway.in_flight")),
        "missing scope discovery line: {:?}",
        report.discovery_lines
    );
    assert!(
        report
            .discovery_lines
            .iter()
            .any(|l| l.starts_with("capacity ") && l.contains("surface=gateway.in_flight")),
        "missing capacity discovery line: {:?}",
        report.discovery_lines
    );
    let cap_line = report
        .discovery_lines
        .iter()
        .find(|l| l.starts_with("capacity "))
        .unwrap();
    assert!(cap_line.contains("util_bp="), "util_bp missing: {cap_line}");

    // Summary line is one greppable token-soup.
    assert!(
        report
            .summary_line
            .starts_with("system=system_api_gateway_limits "),
        "summary_line shape: {}",
        report.summary_line
    );
}

#[test]
fn owner_stop_releases_charges_when_isolate_is_torn_down_mid_flight() {
    // Hold longer than the caller timeout. Every request the gateway
    // admits parks a SharedLease in the isolate's HashMap and starts
    // a sleep. The caller times out before the sleep finishes, so
    // every admitted call is still holding a charge at runtime
    // shutdown. Owner-stop must release them.
    let config = RunConfig {
        upload_callers: 0,
        list_callers: 6,
        list_weight: 1,
        shared_cap: 4,
        list_hold_ms: 1_000,
        call_timeout_ms: 100,
        ..RunConfig::default()
    };
    let report = run(config).expect("gateway run");

    // At least one caller must have admitted (and therefore held a
    // charge across the timeout window). Otherwise we never exercised
    // the "release on isolate teardown" path.
    assert!(
        report.list_admitted >= 1 || report.list_timeout >= 1,
        "expected admit or timeout traffic, got report={report:?}",
    );

    // Post-shutdown snapshot: every admitted weight must have been
    // released by isolate teardown.
    assert_eq!(
        report.scope_current_at_drain, 0,
        "scope.current is non-zero after shutdown — lease leak: report={report:?}",
    );
    assert_eq!(
        report.scope_admitted, report.scope_released,
        "admitted/released drift after shutdown: report={report:?}",
    );
}

#[test]
fn body_bytes_budget_fills_independently_of_in_flight() {
    // Make the in-flight budget loose (cap 100) and the body-bytes budget
    // the binding constraint: cap 1500 with 1024-byte uploads means a
    // second concurrent upload (2048 > 1500) is refused by the body
    // surface, not the in-flight surface.
    let config = RunConfig {
        shared_cap: 100,
        upload_weight: 1,
        list_weight: 1,
        body_cap: 1_500,
        upload_body: 1_024,
        list_body: 128,
        upload_callers: 4,
        list_callers: 0,
        upload_hold_ms: 120,
        call_timeout_ms: 4_000,
        ..RunConfig::default()
    };
    let report = run(config).expect("gateway run");

    assert_eq!(
        report.upload_admitted + report.upload_full,
        4,
        "every upload caller saw a reply: {report:?}",
    );
    // The in-flight budget never binds (cap 100 vs peak weight 4).
    assert_eq!(
        report.scope_full_count, 0,
        "in-flight budget should not be the constraint here: {report:?}",
    );
    // The body-bytes budget IS the constraint.
    assert!(
        report.body_full_count > 0,
        "body-bytes cap 1500 with 1024-byte uploads must refuse some: {report:?}",
    );
    assert_eq!(
        report.body_full_count as usize, report.upload_full,
        "every Full here came from the body surface: {report:?}",
    );
    // Both budgets drain to zero on owner stop.
    assert_eq!(report.scope_current_at_drain, 0, "{report:?}");
    assert_eq!(report.body_current_at_drain, 0, "{report:?}");
    assert_eq!(
        report.body_admitted, report.body_released,
        "every charged byte must be released: {report:?}",
    );
    // The body surface has its own grep-friendly discovery line.
    assert!(
        report
            .discovery_lines
            .iter()
            .any(|l| l.starts_with("scope ") && l.contains("name=gateway.body_bytes")),
        "missing body-bytes discovery line: {:?}",
        report.discovery_lines,
    );
}

#[test]
fn pure_upload_burst_fills_only_upload_lane_then_drains() {
    // No list callers; uploads alone should fill the shared scope
    // because shared_cap=4 / weight=2 = 2 concurrent uploads.
    let config = RunConfig {
        upload_callers: 5,
        list_callers: 0,
        ..RunConfig::default()
    };
    let report = run(config).expect("gateway run");

    assert_eq!(report.upload_admitted + report.upload_full, 5);
    assert!(
        report.scope_full_count > 0,
        "5 uploads against cap=4/weight=2 must see Full — report={report:?}",
    );
    assert_eq!(report.scope_current_at_drain, 0);
}
