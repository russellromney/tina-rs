use specimen_hot_key_fairness::{Report, tina_impl, tokio_impl};

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
        "comparison=specimen_hot_key_fairness side={} hot_admitted={} hot_rejected={} \
         cold_admitted={} cold_rejected={} hot_turns={} cold_min_turns={} \
         cold_min_expected_turns={} max_cold_progress_deficit_turns={} \
         max_progress_gap_turns={} trace_hash={} exit_clean={}",
        side,
        r.hot_admitted,
        r.hot_rejected,
        r.cold_admitted,
        r.cold_rejected,
        r.hot_turns,
        r.cold_min_turns,
        r.cold_min_expected_turns,
        r.max_cold_progress_deficit_turns,
        r.max_progress_gap_turns,
        r.trace_hash,
        r.exit_clean,
    );
    if !r.fairness_line.is_empty() {
        println!("fairness side={} {}", side, r.fairness_line);
    }
}
