use system_lock_manager::{RunConfig, run, run_fifo};

/// Public runner path documented in the README.
#[test]
fn public_smoke() {
    let report = run(RunConfig::default()).expect("lock manager run");
    assert_eq!(report.fifo.admitted_order, vec![1, 2, 3, 4]);
    assert_eq!(report.fifo.grant_order, vec![1, 2, 3, 4]);
    assert!(report.expiry_handoff.waiter_received_grant);
    assert!(report.renewal.final_release_ok);
    assert!(report.stale_release.second_release_was_stale);
    assert_eq!(report.per_key_overflow.busy, 3);
    assert!(report.global_overflow.global_full);
    assert!(report.caller_gone_refill.first_timed_out);
    assert!(report.keyspace_overflow.keyspace_full);
}

/// Accepted default lock-manager workload counts.
#[test]
fn public_characterization() {
    let config = RunConfig::default();
    assert_eq!(config.waiter_capacity, 16);
    assert_eq!(config.max_waiters_per_key, 8);
    assert_eq!(config.max_keys, 64);
    assert_eq!(config.mailbox, 64);
    assert_eq!(config.lease_ms, 200);
    assert_eq!(config.call_timeout_ms, 5_000);

    let fifo = run_fifo(config).expect("fifo characterization");
    assert_eq!(fifo.admitted_order, vec![1, 2, 3, 4]);
    assert_eq!(fifo.grant_order, vec![1, 2, 3, 4]);
    assert_eq!(fifo.stats.acquires_granted, 1);
    assert_eq!(fifo.stats.acquires_handed_off, 4);
    assert_eq!(fifo.stats.releases, 5);
    assert_eq!(fifo.stats.waiters_high_water, 4);
    assert_eq!(fifo.stats.keys_live, 0);
    assert_eq!(fifo.stats.waiters_live, 0);
}
