use specimen_tower_timeout_counter::{Report, tina_impl, tokio_impl};

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
                 usage: specimen-tower-timeout-counter [tokio|tina|both]"
            );
        }
    }
    Ok(())
}

fn print_side(side: &str, report: Report) {
    println!(
        "comparison=specimen_tower_timeout_counter side={} successful={} \
         service_unavailable={} gateway_timeout={} exit_clean={}",
        side,
        report.successful_count,
        report.service_unavailable_count,
        report.gateway_timeout_count,
        report.exit_clean,
    );
}
