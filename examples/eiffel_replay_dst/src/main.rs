use eiffel_replay_dst::{tina_impl, tokio_impl};

fn main() -> anyhow::Result<()> {
    let mode = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "both".to_string());

    match mode.as_str() {
        "tokio" => print_tokio(tokio_impl::run()?),
        "tina" => print_tina(tina_impl::run()?),
        "both" => {
            print_tina(tina_impl::run()?);
            print_tokio(tokio_impl::run()?);
        }
        other => {
            anyhow::bail!(
                "unknown mode {other:?}; expected tokio, tina, or both. usage: eiffel-replay-dst [tokio|tina|both]"
            );
        }
    }
    Ok(())
}

fn print_tina(report: tina_impl::Report) {
    println!(
        "comparison=eiffel_replay_dst side=tina seed_a={} run_a1_events={} run_a2_events={} \
         fingerprints_match={} seed_b={} run_b1_fingerprint_differs={} messages_received={}",
        report.seed_a,
        report.run_a1_event_count,
        report.run_a2_event_count,
        report.run_a1_fingerprint == report.run_a2_fingerprint,
        report.seed_b,
        report.run_b1_fingerprint != report.run_a1_fingerprint,
        report.messages_received,
    );
}

fn print_tokio(report: tokio_impl::Report) {
    let messages_match = report.run1_messages == report.run2_messages;
    let timings_match = report.run1_micros == report.run2_micros;
    println!(
        "comparison=eiffel_replay_dst side=tokio messages_match={messages_match} timings_match={timings_match} \
         run1_us={:?} run2_us={:?}",
        report.run1_micros, report.run2_micros,
    );
}
