use system_lock_manager::{
    MAX_DURATION_MS, MAX_KEYS, MAX_MAILBOX, MAX_WAITERS, RunConfig, RunConfigError,
    run_caller_gone_refill, run_expiry_handoff, run_fifo, run_global_overflow,
    run_keyspace_overflow, run_per_key_overflow, run_renewal, run_stale_release,
};

fn cfg() -> RunConfig {
    RunConfig {
        waiter_capacity: 16,
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
    assert!(report.old_handle_renew_was_stale);
    assert_eq!(report.stats.renewals, 1);
    // No expiry should have fired before the explicit release.
    assert_eq!(report.stats.expiries, 0);
    assert_eq!(report.stats.acquires_handed_off, 1);
    assert_eq!(report.stats.stale_renew_rejects, 1);
    assert_eq!(report.stats.stale_expiries_ignored, 1);
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
    let report = run_per_key_overflow(cfg()).expect("per-key overflow run");
    assert_eq!(report.busy, 3);
    assert_eq!(report.stats.per_key_full_rejects, 3);
    assert_eq!(report.stats.global_full_rejects, 0);
    assert_eq!(report.stats.waiters_live, 0);
}

#[test]
fn global_waiter_capacity_is_distinct_from_per_key_capacity() {
    let report = run_global_overflow(cfg()).expect("global overflow run");
    assert!(report.global_full);
    assert_eq!(report.stats.global_full_rejects, 1);
    assert_eq!(report.stats.per_key_full_rejects, 0);
    assert_eq!(report.stats.waiters_live, 0);
    assert_eq!(report.stats.keys_live, 0);
}

#[test]
fn timed_out_fifo_head_is_reclaimed_and_capacity_refills_exactly() {
    let report = run_caller_gone_refill(cfg()).expect("caller-gone refill run");
    assert!(report.first_timed_out);
    assert!(report.next_waiter_granted);
    assert_eq!(report.stats.waiters_reclaimed, 1);
    assert_eq!(report.stats.waiters_high_water, 1);
    assert_eq!(report.stats.global_full_rejects, 0);
    assert_eq!(report.stats.per_key_full_rejects, 0);
    assert_eq!(report.stats.waiters_live, 0);
    assert_eq!(report.stats.keys_live, 0);
    assert_eq!(report.stats.acquires_handed_off, 1);
    assert_eq!(report.stats.releases, 2);
}

#[test]
fn active_keyspace_cap_rejects_only_new_keys() {
    let report = run_keyspace_overflow(cfg()).expect("keyspace overflow run");
    assert!(report.keyspace_full);
    assert_eq!(report.stats.keyspace_full_rejects, 1);
    assert_eq!(report.stats.keys_live, 0);
    assert_eq!(report.stats.waiters_live, 0);
}

#[test]
fn invalid_bounded_shape_is_fallible() {
    let invalid = [
        (
            RunConfig {
                waiter_capacity: 0,
                ..cfg()
            },
            RunConfigError::Zero {
                field: "waiter_capacity",
            },
        ),
        (
            RunConfig {
                max_waiters_per_key: 0,
                ..cfg()
            },
            RunConfigError::Zero {
                field: "max_waiters_per_key",
            },
        ),
        (
            RunConfig {
                max_keys: 0,
                ..cfg()
            },
            RunConfigError::Zero {
                field: "max_keys",
            },
        ),
        (
            RunConfig {
                mailbox: 0,
                ..cfg()
            },
            RunConfigError::Zero { field: "mailbox" },
        ),
        (
            RunConfig {
                lease_ms: 0,
                ..cfg()
            },
            RunConfigError::Zero { field: "lease_ms" },
        ),
        (
            RunConfig {
                call_timeout_ms: 0,
                ..cfg()
            },
            RunConfigError::Zero {
                field: "call_timeout_ms",
            },
        ),
        (
            RunConfig {
                waiter_capacity: MAX_WAITERS + 1,
                ..cfg()
            },
            RunConfigError::TooLarge {
                field: "waiter_capacity",
                value: MAX_WAITERS + 1,
                max: MAX_WAITERS,
            },
        ),
        (
            RunConfig {
                max_keys: MAX_KEYS + 1,
                ..cfg()
            },
            RunConfigError::TooLarge {
                field: "max_keys",
                value: MAX_KEYS + 1,
                max: MAX_KEYS,
            },
        ),
        (
            RunConfig {
                mailbox: MAX_MAILBOX + 1,
                ..cfg()
            },
            RunConfigError::TooLarge {
                field: "mailbox",
                value: MAX_MAILBOX + 1,
                max: MAX_MAILBOX,
            },
        ),
        (
            RunConfig {
                lease_ms: MAX_DURATION_MS + 1,
                ..cfg()
            },
            RunConfigError::DurationTooLarge {
                field: "lease_ms",
                value_ms: MAX_DURATION_MS + 1,
                max_ms: MAX_DURATION_MS,
            },
        ),
    ];

    for (config, expected) in invalid {
        let error = run_fifo(config).expect_err("invalid config must fail before runtime startup");
        assert_eq!(error.downcast_ref::<RunConfigError>(), Some(&expected));
    }

    assert_eq!(cfg().validate().expect("defaults validate"), cfg());
}
