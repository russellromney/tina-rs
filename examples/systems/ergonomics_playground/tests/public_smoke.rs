//! Public runner proof for the ergonomics playground probes.

use ergonomics_playground::{
    run_debounced_batch_drain_probe, run_debounced_batch_probe, run_quote_race_no_winner_probe,
    run_quote_race_probe, run_single_flight_cache_probe,
};

/// Pins public call-shape facts for every documented probe.
#[test]
fn public_characterization() {
    let quote = run_quote_race_probe().expect("quote race");
    assert!(
        !quote.replies.is_empty(),
        "quote race must reply: {quote:?}"
    );
    assert!(
        quote.late_cancelled_rejections >= 1 || !quote.cancel_outcomes.is_empty(),
        "quote race must settle the loser path: {quote:?}"
    );

    let no_winner = run_quote_race_no_winner_probe().expect("no winner");
    assert!(
        !no_winner.replies.is_empty(),
        "no-winner race must reply: {no_winner:?}"
    );

    let batch = run_debounced_batch_probe().expect("batch");
    assert!(batch.admitted >= 1, "batch must admit callers: {batch:?}");
    assert!(!batch.batch_sizes.is_empty(), "batch must flush: {batch:?}");

    let drained = run_debounced_batch_drain_probe().expect("drain batch");
    assert!(
        drained.closed >= 1 || drained.admitted >= 1,
        "drain probe must observe Closed or admitted work: {drained:?}"
    );

    let cache = run_single_flight_cache_probe().expect("cache");
    assert_eq!(cache.callers, 5);
    assert!(cache.hits >= 1, "cache must hit after fill: {cache:?}");
    assert_eq!(
        cache.upstream_calls, 1,
        "single-flight must call upstream once: {cache:?}"
    );
}

/// Documented public runner path: the five probe functions used by `main`.
#[test]
fn public_smoke() {
    public_characterization();
}
