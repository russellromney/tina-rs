use eiffel_bounded_batcher::{Report, tina_impl, tokio_impl};

fn main() -> anyhow::Result<()> {
    let mode = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "both".to_string());
    match mode.as_str() {
        "tokio" => print_side("tokio", tokio_impl::run()?),
        "tina" => print_side("tina", tina_impl::run()?),
        "both" => {
            print_side("tokio", tokio_impl::run()?);
            print_side("tina", tina_impl::run()?);
        }
        other => anyhow::bail!("unknown mode {other:?}"),
    }
    Ok(())
}

fn print_side(side: &str, r: Report) {
    println!(
        "comparison=eiffel_bounded_batcher side={} callers={} successes={} \
         full={} failed={} size_flushes={} timer_flushes={} exit_clean={}",
        side,
        r.callers,
        r.successes,
        r.full_rejects,
        r.failed,
        r.batches_size_flushed,
        r.batches_timer_flushed,
        r.exit_clean,
    );
}
