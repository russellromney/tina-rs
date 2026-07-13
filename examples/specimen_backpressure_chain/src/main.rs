use specimen_backpressure_chain::{Report, tina_impl, tokio_impl};

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
                 usage: specimen-backpressure-chain [tokio|tina|both]"
            );
        }
    }
    Ok(())
}

fn print_side(side: &str, report: Report) {
    println!(
        "comparison=specimen_backpressure_chain side={} successful={} \
         c_timed_out={} b_timed_out={} caller_timeout={} full={} closed={} rejected={} \
         domain_failure={} runtime_failure={} exit_clean={}",
        side,
        report.successful,
        report.c_timed_out,
        report.b_timed_out,
        report.caller_timeout,
        report.full,
        report.closed,
        report.rejected,
        report.domain_failure,
        report.runtime_failure,
        report.exit_clean,
    );
}
