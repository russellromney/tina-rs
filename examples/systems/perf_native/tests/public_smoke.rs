use perf_native::{WorkloadConfig, WorkloadConfigError, host_call_compare_with};

/// Public comparison entry path: validate config, then drive a row from it.
#[test]
fn public_smoke() {
    let config = WorkloadConfig::default()
        .validate()
        .expect("accepted default workload");
    assert_eq!(config.ops, 120);
    assert_eq!(config.http_ops, 32);
    assert_eq!(config.workers, 4);
    assert_eq!(config.samples, 5);
    assert_eq!(config.capacity, 184);

    // Pass the same validated config into the comparison row so ops/workers
    // actually gate construction rather than being ignored after validate().
    let report = host_call_compare_with(config).expect("host call comparison");
    assert_eq!(report.label, "host_request_reply");
    assert_eq!(
        report.tina.load.ops_attempted,
        report.baseline.load.ops_attempted
    );
    assert_eq!(report.tina.load.ops_attempted, config.ops);
    assert!(report.tina.load.ops_ok > 0);
    assert_eq!(report.tina.load.ops_err, 0);
    assert_eq!(report.tina.load.ops_timeout, 0);
}

/// Accepted workload counts for the comparison rows.
#[test]
fn public_characterization() {
    let config = WorkloadConfig::default();
    assert_eq!(config.ops, 120);
    assert_eq!(config.http_ops, 32);
    assert_eq!(config.workers, 4);
    assert_eq!(config.samples, 5);
    assert_eq!(config.capacity, 184);
    assert_eq!(config.call_timeout_ms, 2_000);
    assert_eq!(config.validate().expect("defaults"), config);

    assert!(matches!(
        WorkloadConfig {
            ops: 0,
            ..WorkloadConfig::default()
        }
        .validate(),
        Err(WorkloadConfigError::Zero { field: "ops" })
    ));
    assert!(matches!(
        WorkloadConfig {
            capacity: 10,
            ops: 11,
            ..WorkloadConfig::default()
        }
        .validate(),
        Err(WorkloadConfigError::CapacityTooSmall {
            capacity: 10,
            ops: 11
        })
    ));
    assert!(matches!(
        WorkloadConfig {
            ops: u64::MAX,
            capacity: usize::MAX,
            ..WorkloadConfig::default()
        }
        .validate(),
        Err(WorkloadConfigError::TooLarge { field: "ops", .. })
    ));

    // Invalid config is rejected by the comparison entry itself.
    let error = host_call_compare_with(WorkloadConfig {
        ops: 0,
        ..WorkloadConfig::default()
    })
    .expect_err("zero ops must fail before runtime construction");
    assert!(
        error.to_string().contains("ops"),
        "unexpected error: {error:#}"
    );

    // Non-default knobs must round-trip into the report. Hardcoding private
    // OPS/SAMPLES constants inside the row would still pass a default-only smoke.
    let custom = WorkloadConfig {
        ops: 8,
        http_ops: 4,
        workers: 1,
        samples: 1,
        capacity: 16,
        call_timeout_ms: 2_000,
    }
    .validate()
    .expect("small accepted workload");
    let report = host_call_compare_with(custom).expect("custom host call comparison");
    assert_eq!(report.tina.load.ops_attempted, 8, "ops must come from config");
    assert_eq!(
        report.baseline.load.ops_attempted, 8,
        "baseline must share config ops"
    );
    assert_eq!(report.samples, 1, "samples must come from config");
    assert!(report.tina.load.ops_ok > 0, "custom row must do useful work");
}
