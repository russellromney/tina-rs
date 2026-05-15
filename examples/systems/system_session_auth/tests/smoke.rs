use system_session_auth::{RunConfig, run};

#[test]
fn login_touch_logout_walks_session_lifecycle_and_idle_sweep_expires_state() {
    let report = run(RunConfig {
        shards: 4,
        max_sessions_per_shard: 16,
        idle_timeout_ms: 80,
        sweep_interval_ms: 20,
        session_mailbox: 128,
        call_timeout_ms: 2_000,
    })
    .expect("run succeeds");

    // 1. Login admits, touch ticks the row, logout drops the slot,
    //    and a follow-up touch reports NotFound.
    let ltl = &report.login_touch_logout;
    assert!(ltl.login_ok);
    assert!(ltl.touch_ok);
    assert!(ltl.logout_ok);
    assert!(ltl.touch_after_logout_not_found);
    assert_eq!(ltl.stats.admitted, 1);
    assert_eq!(ltl.stats.logged_out, 1);
    assert_eq!(ltl.stats.touch_ok, 1);
    assert_eq!(ltl.stats.touch_not_found, 1);
    assert_eq!(ltl.stats.active, 0);
    assert_eq!(ltl.stats.idle_expired, 0);
    assert_eq!(ltl.stats.full_rejects, 0);

    // 2. Idle expiry: one admitted session, no touch, sweep removes it
    //    before the test polls. NotFound is returned; the stats show
    //    the sweep actually ran and counted the expiry.
    let idle = &report.idle_expiry;
    assert!(idle.touch_after_idle_not_found);
    assert_eq!(idle.stats.admitted, 1);
    assert_eq!(idle.stats.idle_expired, 1);
    assert_eq!(idle.stats.active, 0);
    assert!(
        idle.stats.sweeps_run >= 2,
        "expected at least 2 sweeps, saw {}",
        idle.stats.sweeps_run
    );

    // 3. Overflow: per-bucket cap is enforced. Capacity is reported.
    let ov = &report.overflow;
    assert_eq!(ov.admitted, 4);
    assert_eq!(ov.full, 5);
    assert_eq!(ov.stats.full_rejects, 5);
    assert_eq!(ov.stats.per_shard_high_water, vec![4]);
    assert_eq!(ov.stats.per_shard_active, vec![4]);
}
