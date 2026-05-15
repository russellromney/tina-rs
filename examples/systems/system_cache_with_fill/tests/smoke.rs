use system_cache_with_fill::{RunConfig, run};

#[test]
fn cache_fill_is_single_flight_and_stale_results_do_not_cache() {
    let report = run(RunConfig {
        callers: 8,
        pending_capacity: 5,
        cache_mailbox: 64,
        fill_ms: 80,
        call_timeout_ms: 2_000,
    })
    .expect("run succeeds");

    assert_eq!(report.single_flight.callers, 8);
    assert_eq!(report.single_flight.filled, 5);
    assert_eq!(report.single_flight.busy, 3);
    assert_eq!(report.single_flight.failed, 0);
    assert!(report.single_flight.hit_after_fill);
    assert_eq!(report.single_flight.stats.fills_started, 1);
    assert_eq!(report.single_flight.stats.fills_completed, 1);
    assert_eq!(report.single_flight.stats.pending_high_water, 5);
    assert_eq!(report.single_flight.stats.pending_full_rejects, 3);

    assert!(report.stale_invalidation.first_get_stale);
    assert!(report.stale_invalidation.replacement_filled);
    assert_eq!(report.stale_invalidation.stats.invalidations, 1);
    assert_eq!(report.stale_invalidation.stats.stale_replies, 1);
    assert_eq!(report.stale_invalidation.stats.stale_completions, 1);
    assert_eq!(report.stale_invalidation.stats.fills_started, 2);
    assert_eq!(report.stale_invalidation.stats.fills_completed, 1);
}
