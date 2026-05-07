use eiffel_hot_key_fairness::{Report, tina_impl, tokio_impl};

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
        "comparison=eiffel_hot_key_fairness side={} hot_admitted={} hot_rejected={} \
         cold_admitted={} cold_rejected={} exit_clean={}",
        side, r.hot_admitted, r.hot_rejected, r.cold_admitted, r.cold_rejected, r.exit_clean,
    );
}
