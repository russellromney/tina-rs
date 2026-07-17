use perf_native::{WorkloadConfig, WorkloadConfigError, host_call_compare};

/// Public comparison entry path.
#[test]
fn public_smoke() {
    let config = WorkloadConfig::default()
        .validate()
        .expect("accepted default workload");
    assert_eq!(config.ops, 120);
    assert_eq!(config.workers, 4);
    assert_eq!(config.samples, 5);
    assert_eq!(config.capacity, 184);

    let report = host_call_compare().expect("host call comparison");
    assert_eq!(report.label, "host_request_reply");
    assert_eq!(
        report.tina.load.ops_attempted,
        report.baseline.load.ops_attempted
    );
    assert!(report.tina.load.ops_ok > 0);
    assert_eq!(report.tina.load.ops_err, 0);
    assert_eq!(report.tina.load.ops_timeout, 0);
}

/// Accepted workload counts for the comparison rows.
#[test]
fn public_characterization() {
    let config = WorkloadConfig::default();
    assert_eq!(config.ops, 120);
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
}
