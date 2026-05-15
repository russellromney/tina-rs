fn main() -> anyhow::Result<()> {
    let quote = ergonomics_playground::run_quote_race_probe()?;
    println!(
        "quote_race replies={:?} cancel_outcomes={} late_cancelled_rejections={} rough_edges={:?}",
        quote.replies, quote.cancel_outcomes, quote.late_cancelled_rejections, quote.rough_edges
    );

    let no_winner = ergonomics_playground::run_quote_race_no_winner_probe()?;
    println!(
        "quote_race_no_winner replies={:?} cancel_outcomes={} rough_edges={:?}",
        no_winner.replies, no_winner.cancel_outcomes, no_winner.rough_edges
    );

    let batch = ergonomics_playground::run_debounced_batch_probe()?;
    println!(
        "debounced_batch admitted={} full={} closed={} batch_sizes={:?} sums={:?} rough_edges={:?}",
        batch.admitted, batch.full, batch.closed, batch.batch_sizes, batch.sums, batch.rough_edges
    );

    let drained = ergonomics_playground::run_debounced_batch_drain_probe()?;
    println!(
        "debounced_batch_drain admitted={} full={} closed={} rough_edges={:?}",
        drained.admitted, drained.full, drained.closed, drained.rough_edges
    );

    let cache = ergonomics_playground::run_single_flight_cache_probe()?;
    println!(
        "single_flight_cache callers={} hits={} full={} upstream_calls={} values={:?} rough_edges={:?}",
        cache.callers,
        cache.hits,
        cache.full,
        cache.upstream_calls,
        cache.values,
        cache.rough_edges
    );

    Ok(())
}
