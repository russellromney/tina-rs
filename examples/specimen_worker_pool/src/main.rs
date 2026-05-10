use specimen_worker_pool::{Report, tina_impl, tokio_impl};

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
        "comparison=specimen_worker_pool side={} clients={} correct={} wrong={} failed={} exit_clean={}",
        side, r.clients, r.correct_replies, r.wrong_replies, r.failed, r.exit_clean,
    );
}
