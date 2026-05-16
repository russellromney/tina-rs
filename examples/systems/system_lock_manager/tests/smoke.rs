use system_lock_manager::{
    RunConfig, run_busy_overflow, run_expiry_handoff, run_fifo, run_renewal, run_stale_release,
};

fn cfg() -> RunConfig {
    RunConfig {
        pending_capacity: 16,
        max_waiters_per_key: 8,
        max_keys: 64,
        mailbox: 64,
        lease_ms: 200,
        call_timeout_ms: 5_000,
    }
}

#[test]
fn fifo_grant_order_matches_admission_order() {
    let report = run_fifo(cfg()).expect("fifo run");
    assert_eq!(report.admitted_order, vec![1, 2, 3, 4]);
    assert_eq!(report.grant_order, vec![1, 2, 3, 4]);
    // 1 immediate grant for the host holder, 4 hand-offs to the
    // contenders.
    assert_eq!(report.stats.acquires_granted, 1);
    assert_eq!(report.stats.acquires_handed_off, 4);
    assert_eq!(report.stats.releases, 5);
    assert_eq!(report.stats.expiries, 0);
    assert_eq!(report.stats.stale_release_rejects, 0);
    assert_eq!(report.stats.waiters_high_water, 4);
    assert_eq!(report.stats.keys_live, 0);
    assert_eq!(report.stats.waiters_live, 0);
}

#[test]
fn lease_expiry_hands_off_and_marks_old_handle_stale() {
    let report = run_expiry_handoff(cfg()).expect("expiry run");
    assert!(report.waiter_received_grant);
    assert!(report.original_release_was_stale);
    assert_eq!(report.stats.expiries, 1);
    assert_eq!(report.stats.acquires_handed_off, 1);
    assert_eq!(report.stats.stale_release_rejects, 1);
}

#[test]
fn renewal_extends_lease_and_keeps_waiter_parked() {
    let report = run_renewal(cfg()).expect("renewal run");
    assert!(report.still_held_after_original_lease);
    assert!(report.final_release_ok);
    assert_eq!(report.stats.renewals, 1);
    // No expiry should have fired before the explicit release.
    assert_eq!(report.stats.expiries, 0);
    assert_eq!(report.stats.acquires_handed_off, 1);
}

#[test]
fn second_release_with_stale_handle_is_rejected() {
    let report = run_stale_release(cfg()).expect("stale run");
    assert!(report.second_release_was_stale);
    assert_eq!(report.stats.stale_release_rejects, 1);
    assert_eq!(report.stats.releases, 1);
}

#[test]
fn per_key_wait_queue_overflows_to_busy() {
    let report = run_busy_overflow(cfg()).expect("busy run");
    assert_eq!(report.busy, 3);
    assert_eq!(report.stats.per_key_full_rejects, 3);
}
