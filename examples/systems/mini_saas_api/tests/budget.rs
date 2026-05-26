//! The budget manifest is the copied service's single source of caps.
//!
//! These tests pin the four claims that make the manifest trustworthy:
//! it validates before startup, every documented cap is a row, every
//! live surface has a row, and the replay export pins the caps a saved
//! case depends on.

use mini_saas_api::{RunMode, budget, run};
use tina::capacity::CapacityPolicy;
use tina_runtime::budget::{
    BudgetCap, BudgetConsistencyRow, BudgetKind, BudgetSurface, BudgetUnit, BudgetValidationError,
    ReplayImpact, ServiceBudgetManifest,
};

#[test]
fn manifest_validates_before_startup() {
    budget::manifest()
        .validate()
        .expect("the service's own manifest must validate");
}

#[test]
fn invalid_manifest_fails_early_with_typed_errors() {
    // A zero body cap would fake EOF / reject every body. Validation
    // must reject it with a typed error before any socket binds.
    let mut bad = ServiceBudgetManifest::new("mini_saas_api", CapacityPolicy::Production)
        .require_kind(BudgetKind::BridgeInFlight);
    bad.add(BudgetSurface::new(
        "http.request_body",
        BudgetKind::BodyBytes,
        BudgetUnit::Bytes,
        BudgetCap::fixed(0),
    ));
    let errors = bad
        .validate()
        .expect_err("zero cap + missing kind must fail");
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, BudgetValidationError::ZeroCap { .. })),
        "expected a typed ZeroCap error, got {errors:?}"
    );
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, BudgetValidationError::MissingRequiredKind { .. })),
        "expected a typed MissingRequiredKind error, got {errors:?}"
    );
}

#[test]
fn every_documented_cap_is_a_manifest_row() {
    let manifest = budget::manifest();
    for (name, cap) in budget::documented_caps() {
        assert_eq!(
            manifest.cap_max(name),
            Some(cap),
            "documented cap {name}={cap} must be a manifest row with that value",
        );
    }
    // And no manifest row is undocumented: the README/example and the
    // manifest cannot drift in either direction.
    assert_eq!(
        manifest.len(),
        budget::documented_caps().len(),
        "every manifest surface must be documented"
    );
}

#[test]
fn live_surfaces_all_have_manifest_rows_and_observed_facts() {
    let report = run(RunMode::Smoke).expect("smoke run");
    assert!(
        report.budget_consistent,
        "manifest vs live mismatch: {:?}",
        report.budget_report.as_ref().map(|r| &r.consistency.rows),
    );
    let budget_report = report.budget_report.expect("budget report present");

    // No live surface is missing a manifest row (no Extra rows).
    assert!(
        budget_report
            .consistency
            .rows
            .iter()
            .all(|r| !matches!(r, BudgetConsistencyRow::Extra { .. })),
        "a live surface had no manifest row: {:?}",
        budget_report.consistency.rows,
    );

    // The public body surface carries observed facts from BodyMetrics,
    // not guessed config: the body-cap exceed earlier in the run must
    // show as a full count.
    let body = budget_report
        .row("http.request_body")
        .expect("body row")
        .observed
        .as_ref()
        .expect("body observed");
    assert_eq!(body.dimension, "weight");
    assert!(
        body.full_count >= 1,
        "the 413 path should record a body full"
    );

    // The db in-flight surface is measured from the bridge report, not
    // from declared config.
    assert!(
        budget_report
            .row("db.in_flight")
            .expect("db row")
            .observed
            .is_some(),
        "db in-flight should be measured from the bridge report",
    );
}

#[test]
fn request_scope_surfaces_declared_and_joined_with_live_pressure() {
    // Declared: the scope-set capacity and the per-request child cap are
    // manifest rows of the RequestScope kind, not scattered constants.
    let manifest = budget::manifest();
    let set = manifest
        .surface("request.scope_set")
        .expect("request.scope_set must be a manifest row");
    assert_eq!(set.kind, BudgetKind::RequestScope);
    let child = manifest
        .surface("request.scope_child_cap")
        .expect("request.scope_child_cap must be a manifest row");
    assert_eq!(child.kind, BudgetKind::RequestScope);

    // Joined with live pressure: after a run that drove notify requests,
    // the scope-set surface carries observed facts (a non-zero high-water,
    // since the notify path admitted at least one scope), and every slot
    // came back (current == 0, full == 0).
    let report = run(RunMode::Smoke).expect("smoke run");
    assert!(
        report.budget_consistent,
        "manifest vs live mismatch: {:?}",
        report.budget_report.as_ref().map(|r| &r.consistency.rows),
    );
    let budget_report = report.budget_report.expect("budget report present");
    let scope_row = budget_report
        .row("request.scope_set")
        .expect("scope-set row")
        .observed
        .as_ref()
        .expect("scope-set observed facts joined from live metrics");
    assert!(
        scope_row.high_water >= 1,
        "the notify path should admit at least one request scope: {scope_row:?}",
    );
    assert_eq!(
        scope_row.current, 0,
        "every notify scope should be retired by shutdown: {scope_row:?}",
    );
    assert_eq!(
        scope_row.full_count, 0,
        "the smoke run should not exhaust the scope set: {scope_row:?}",
    );
}

#[test]
fn replay_export_pins_body_cap_and_ignores_display_only() {
    let manifest = budget::manifest();
    assert_eq!(
        manifest.surface("http.request_body").unwrap().replay_impact,
        ReplayImpact::ReplayAffecting,
    );
    assert_eq!(
        manifest
            .surface("http.main_listener.mailbox")
            .unwrap()
            .replay_impact,
        ReplayImpact::DisplayOnly,
    );

    let export = manifest.replay_export();
    assert!(
        export
            .replay_affecting
            .iter()
            .any(|e| e.name == "http.request_body"),
        "the body cap must be hashed into the replay export",
    );
    assert!(
        export
            .ignored_for_replay
            .contains(&"http.main_listener.mailbox".to_string()),
        "the accept-queue depth must be ignored for replay",
    );
}
