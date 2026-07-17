use system_copied_service_path::{RunConfig, run};

/// Public runner path documented in the README.
#[test]
fn public_smoke() {
    let report = run(RunConfig::default()).expect("copied service path run");
    assert!(report.admitted > 0, "report={report:?}");
    assert!(report.full > 0, "report={report:?}");
    assert_eq!(
        report.admitted + report.full,
        RunConfig::default().callers,
        "report={report:?}"
    );
    assert_eq!(
        report.ledger_final_len,
        report.ledger_seed_len + report.admitted,
        "report={report:?}"
    );
    assert_eq!(report.scope_current_at_drain, 0, "report={report:?}");
    assert_eq!(report.scope_admitted, report.scope_released, "report={report:?}");
    assert!(report.load.leak_checked && report.load.leak_clean, "report={report:?}");
}

/// Accepted default workload counts before any migration that might change them.
#[test]
fn public_characterization() {
    let config = RunConfig::default();
    assert_eq!(config.capacity, 2);
    assert_eq!(config.mailbox, 32);
    assert_eq!(config.work_ms, 40);
    assert_eq!(config.callers, 6);
    assert_eq!(config.call_timeout_ms, 2_000);
    assert!(config.timer_capacity > 0);

    let report = run(config).expect("characterization run");
    assert_eq!(report.ledger_seed_len, 1, "report={report:?}");
    assert_eq!(
        report.admitted + report.full,
        6,
        "default callers must all settle: {report:?}"
    );
    assert!(report.admitted <= config.capacity || report.full > 0, "report={report:?}");
    assert_eq!(report.scope_current_at_drain, 0, "report={report:?}");
}
