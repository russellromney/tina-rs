use specimen_webhook_outbox::{Report, hand_impl, tina_impl};

fn main() -> anyhow::Result<()> {
    let mode = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "both".to_string());

    match mode.as_str() {
        "hand" => print_side("hand", hand_impl::run()?),
        "tina" => print_side("tina", tina_impl::run()?),
        "both" => {
            print_side("hand", hand_impl::run()?);
            print_side("tina", tina_impl::run()?);
        }
        other => {
            anyhow::bail!(
                "unknown mode {other:?}; expected hand, tina, or both. \
                 usage: specimen-webhook-outbox [hand|tina|both]"
            );
        }
    }
    Ok(())
}

fn print_side(side: &str, report: Report) {
    println!(
        "comparison=specimen_webhook_outbox side={} \
         phase_a_sent={} phase_a_marked={} recovered_pending={} \
         phase_b_resent={} final_marked={} \
         journal_before={} journal_after={} exit_clean={}",
        side,
        report.phase_a_sent,
        report.phase_a_marked,
        report.recovered_pending,
        report.phase_b_resent,
        report.final_marked,
        report.journal_records_before_compaction,
        report.journal_records_after_compaction,
        report.exit_clean,
    );
}
