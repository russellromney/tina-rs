use specimen_mux_client::{Report, tina_impl, tokio_impl};

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
                "unknown mode {other:?}; expected tokio, tina, or both. usage: specimen-mux-client [tokio|tina|both]"
            );
        }
    }
    Ok(())
}

fn print_side(side: &str, report: Report) {
    println!(
        "comparison=specimen_mux_client side={} arrival_order={:?}",
        side, report.arrival_order,
    );
}
