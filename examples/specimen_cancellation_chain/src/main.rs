use specimen_cancellation_chain::{Report, tina_impl, tokio_impl};

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
        "comparison=specimen_cancellation_chain side={} replies_before_cancel={} \
         replies_after_cancel={} call_full={} call_closed={} call_timeout={} call_rejected={} \
         cancel_cancelled={} cancel_not_admitted={} cancel_already_completed={} \
         cancel_already_cancelled={} cancel_wrong_shard={} pending={} \
         settlement_complete={} settlement_protocol_errors={} cancel_observed={} exit_clean={}",
        side,
        report.replies_before_cancel,
        report.replies_after_cancel,
        report.call_full,
        report.call_closed,
        report.call_timeout,
        report.call_rejected,
        report.cancel_cancelled,
        report.cancel_not_admitted,
        report.cancel_already_completed,
        report.cancel_already_cancelled,
        report.cancel_wrong_shard,
        report.pending,
        report.settlement_complete,
        report.settlement_protocol_errors,
        report.cancel_observed,
        report.exit_clean,
    );
}
