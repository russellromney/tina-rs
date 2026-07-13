use specimen_dynamic_worker_pool::{Report, tina_impl, tokio_impl};

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
        other => {
            anyhow::bail!(
                "unknown mode {other:?}; expected tokio, tina, or both. \
                 usage: specimen-dynamic-worker-pool [tokio|tina|both]"
            );
        }
    }
    Ok(())
}

fn print_side(side: &str, report: Report) {
    println!(
        "comparison=specimen_dynamic_worker_pool side={} results_collected={} \
         total_sum={} spawn_zero_capacity={} spawn_destination_unavailable={} spawn_other={} \
         call_full={} call_closed={} call_timeout={} \
         rejected_foreign_system={} rejected_reply_abandoned={} \
         rejected_handler_panicked={} rejected_unsupported_message={} exit_clean={}",
        side,
        report.results_collected,
        report.total_sum,
        report.spawn_zero_capacity,
        report.spawn_destination_unavailable,
        report.spawn_other,
        report.call_full,
        report.call_closed,
        report.call_timeout,
        report.rejected_foreign_system,
        report.rejected_reply_abandoned,
        report.rejected_handler_panicked,
        report.rejected_unsupported_message,
        report.exit_clean,
    );
}
