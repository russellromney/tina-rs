use eiffel_cancellation_chain::{Report, tina_impl, tokio_impl};

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
        other => anyhow::bail!("unknown mode {other:?}; expected tokio, tina, or both"),
    }
    Ok(())
}

fn print_side(side: &str, report: Report) {
    println!(
        "comparison=eiffel_cancellation_chain side={} replies_before_cancel={} \
         replies_after_cancel={} cancel_observed={} exit_clean={}",
        side,
        report.replies_before_cancel,
        report.replies_after_cancel,
        report.cancel_observed,
        report.exit_clean,
    );
}
