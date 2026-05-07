use eiffel_graceful_shutdown::{Report, tina_impl, tokio_impl};

/// Each side installs its own process-wide signal handler. Running
/// both in the same process means one side's handler fires during
/// the other's run, so we deliberately do not expose a `both` mode
/// here — run them as separate processes (or rely on `cargo test`
/// running the per-side smoke tests, each in its own process).
fn main() -> anyhow::Result<()> {
    let mode = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "tokio".to_string());

    match mode.as_str() {
        "tokio" => print_side("tokio", tokio_impl::run()?),
        "tina" => print_side("tina", tina_impl::run()?),
        other => {
            anyhow::bail!(
                "unknown mode {other:?}; expected tokio or tina (no `both` — see lib.rs). \
                 usage: eiffel-graceful-shutdown [tokio|tina]"
            );
        }
    }
    Ok(())
}

fn print_side(side: &str, report: Report) {
    println!(
        "comparison=eiffel_graceful_shutdown side={} items_produced={} items_processed={} \
         signal_received={} remaining_in_queue={} exit_clean={}",
        side,
        report.items_produced,
        report.items_processed,
        report.signal_received,
        report.items_remaining_in_queue_at_exit,
        report.exit_clean,
    );
}
