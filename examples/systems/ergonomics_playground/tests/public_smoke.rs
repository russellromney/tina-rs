//! Public runner proof for the ergonomics playground probes.

use ergonomics_playground::{
    QuoteReply, run_debounced_batch_drain_probe, run_debounced_batch_probe,
    run_quote_race_no_winner_probe, run_quote_race_probe, run_single_flight_cache_probe,
};
use tina::CancelOutcome;

/// Pins the exact deterministic facts every documented probe must
/// produce on the Simulator (the same vectors the lib tests assert).
#[test]
fn public_characterization() {
    let quote = run_quote_race_probe().expect("quote race");
    assert_eq!(
        quote.replies,
        vec![QuoteReply::Quote {
            provider: "fast",
            cents: 525,
        }],
        "quote race winner reply: {quote:?}"
    );
    assert_eq!(
        quote.cancel_outcomes,
        vec![CancelOutcome::Cancelled],
        "quote race loser cancel: {quote:?}"
    );
    assert_eq!(
        quote.late_cancelled_rejections, 1,
        "quote race late-cancel rejection count: {quote:?}"
    );
    assert!(quote.rough_edges.is_empty(), "{quote:?}");

    let no_winner = run_quote_race_no_winner_probe().expect("no winner");
    assert_eq!(
        no_winner.replies,
        vec![QuoteReply::Unavailable],
        "no-winner race reply: {no_winner:?}"
    );
    assert!(
        no_winner.cancel_outcomes.is_empty(),
        "no-winner race issues no cancels: {no_winner:?}"
    );
    assert_eq!(
        no_winner.late_cancelled_rejections, 0,
        "no-winner race late-cancel rejection count: {no_winner:?}"
    );
    assert!(no_winner.rough_edges.is_empty(), "{no_winner:?}");

    let batch = run_debounced_batch_probe().expect("batch");
    assert_eq!(batch.admitted, 3, "{batch:?}");
    assert_eq!(batch.full, 2, "{batch:?}");
    assert_eq!(batch.closed, 0, "{batch:?}");
    assert_eq!(batch.timer_failed, 0, "{batch:?}");
    assert_eq!(batch.call_full, 0, "{batch:?}");
    assert_eq!(batch.call_closed, 0, "{batch:?}");
    assert_eq!(batch.call_timeout, 0, "{batch:?}");
    assert_eq!(batch.call_rejected, 0, "{batch:?}");
    assert_eq!(batch.batch_ids, vec![1, 1, 1], "{batch:?}");
    assert_eq!(batch.batch_sizes, vec![3, 3, 3], "{batch:?}");
    assert_eq!(batch.sums, vec![10, 10, 10], "{batch:?}");
    assert!(batch.rough_edges.is_empty(), "{batch:?}");

    let drained = run_debounced_batch_drain_probe().expect("drain batch");
    assert_eq!(drained.admitted, 0, "{drained:?}");
    assert_eq!(drained.full, 0, "{drained:?}");
    assert_eq!(drained.closed, 3, "{drained:?}");
    assert_eq!(drained.timer_failed, 0, "{drained:?}");
    assert_eq!(drained.call_full, 0, "{drained:?}");
    assert_eq!(drained.call_closed, 0, "{drained:?}");
    assert_eq!(drained.call_timeout, 0, "{drained:?}");
    assert_eq!(drained.call_rejected, 0, "{drained:?}");
    assert!(drained.batch_ids.is_empty(), "{drained:?}");
    assert!(drained.rough_edges.is_empty(), "{drained:?}");

    let cache = run_single_flight_cache_probe().expect("cache");
    assert_eq!(cache.callers, 5, "{cache:?}");
    assert_eq!(cache.hits, 3, "{cache:?}");
    assert_eq!(cache.full, 2, "{cache:?}");
    assert_eq!(
        cache.upstream_calls, 1,
        "single-flight must call upstream once: {cache:?}"
    );
    assert_eq!(cache.values, vec![42, 42, 42], "{cache:?}");
    assert!(cache.rough_edges.is_empty(), "{cache:?}");
}

/// Documented public runner path: the five probe functions used by `main`.
#[test]
fn public_smoke() {
    public_characterization();
}
