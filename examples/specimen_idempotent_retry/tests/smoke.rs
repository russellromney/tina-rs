use specimen_idempotent_retry::{Outcome, RunConfig, run};

#[test]
fn bounded_retry_eventually_delivers_and_charges_once() {
    // Downstream rejects twice, budget allows 4 retries → delivered on the
    // third attempt, charged exactly once (idempotent).
    let config = RunConfig {
        downstream_fail_first: 2,
        max_retries: 4,
        ..RunConfig::default()
    };
    let report = run(config).expect("run");
    assert_eq!(report.outcome, Outcome::Delivered, "got {report:?}");
    assert_eq!(report.attempts, 3, "first try + 2 retries: {report:?}");
    assert_eq!(report.retries, 2, "got {report:?}");
    assert_eq!(
        report.downstream_charges, 1,
        "idempotent: exactly one charge even across retries: {report:?}"
    );
    assert_eq!(report.idempotency_key, config.idempotency_key);
}

#[test]
fn retry_budget_exhaustion_is_visible() {
    // Downstream rejects more times than the budget allows → exhausted,
    // and the downstream never committed a charge.
    let config = RunConfig {
        downstream_fail_first: 10,
        max_retries: 2,
        ..RunConfig::default()
    };
    let report = run(config).expect("run");
    assert_eq!(report.outcome, Outcome::Exhausted, "got {report:?}");
    // First attempt + 2 retries = 3 attempts, all rejected.
    assert_eq!(report.attempts, 3, "got {report:?}");
    assert_eq!(
        report.retries, 2,
        "budget granted exactly 2 retries: {report:?}"
    );
    assert_eq!(
        report.downstream_charges, 0,
        "exhausted delivery must not have charged: {report:?}"
    );
}

#[test]
fn first_attempt_success_uses_no_retries() {
    let config = RunConfig {
        downstream_fail_first: 0,
        ..RunConfig::default()
    };
    let report = run(config).expect("run");
    assert_eq!(report.outcome, Outcome::Delivered);
    assert_eq!(report.attempts, 1);
    assert_eq!(report.retries, 0);
    assert_eq!(report.downstream_charges, 1);
}
