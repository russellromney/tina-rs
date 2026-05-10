use specimen_persistent_counter::{Report, tina_impl, tokio_impl};

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
                "unknown mode {other:?}; expected tokio, tina, or both. usage: specimen-persistent-counter [tokio|tina|both]"
            );
        }
    }
    Ok(())
}

fn print_side(side: &str, report: Report) {
    println!(
        "comparison=specimen_persistent_counter side={} \
         phase_a_final={} snapshot_committed={} \
         phase_b_recovered={} phase_b_final={} \
         journal_records_phase_b={} exit_clean={}",
        side,
        report.phase_a_final,
        report.snapshot_committed,
        report.phase_b_recovered,
        report.phase_b_final,
        report.journal_records_phase_b,
        report.exit_clean,
    );
}
