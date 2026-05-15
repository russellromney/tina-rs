fn main() -> anyhow::Result<()> {
    let quote = ergonomics_playground::run_quote_race_probe()?;
    println!(
        "quote_race replies={:?} cancel_outcomes={} rough_edges={:?}",
        quote.replies, quote.cancel_outcomes, quote.rough_edges
    );

    let batch = ergonomics_playground::run_debounced_batch_probe()?;
    println!(
        "debounced_batch admitted={} full={} batch_sizes={:?} sums={:?} rough_edges={:?}",
        batch.admitted, batch.full, batch.batch_sizes, batch.sums, batch.rough_edges
    );

    Ok(())
}
