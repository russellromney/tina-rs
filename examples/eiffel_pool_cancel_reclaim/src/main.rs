use eiffel_pool_cancel_reclaim::{Report, tina_impl, tokio_impl};

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
        "comparison=eiffel_pool_cancel_reclaim side={} cancelled={} \
         retried_admitted={} retried_full={} retried_resourced={} \
         exit_clean={}",
        side,
        r.cancelled,
        r.retried_admitted,
        r.retried_full,
        r.retried_resourced,
        r.exit_clean,
    );
}
