use specimen_replay_dst::{tina_impl, tokio_impl};

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
                "unknown mode {other:?}; expected tokio, tina, or both. usage: specimen-replay-dst [tokio|tina|both]"
            );
        }
    }
    Ok(())
}

fn print_tina(demo: tina_impl::Demo) {
    let case = tina_impl::case();
    println!("comparison=specimen_replay_dst side=tina");
    println!(
        "  saved case: name={:?} seed={} events={} hash=0x{:016x} \
         scenario={:?} invariant={:?} messages_received={}",
        case.name,
        case.seed,
        demo.saved.event_count,
        demo.saved.trace_hash,
        case.scenario,
        case.invariant,
        demo.saved.output.messages_received,
    );
    match &demo.sweep {
        Ok(success) => println!(
            "  sweep: name={:?} seeds_examined={} (no failure)",
            success.name, success.seeds_examined
        ),
        Err(failure) => println!("  sweep failure (paste this):\n{failure}"),
    }
    println!(
        "  shrink (paste this if it shrunk smaller):\n{}",
        demo.shrink
    );
}

fn print_tokio(report: tokio_impl::Report) {
    let messages_match = report.run1_messages == report.run2_messages;
    let timings_match = report.run1_micros == report.run2_micros;
    println!(
        "comparison=specimen_replay_dst side=tokio messages_match={messages_match} timings_match={timings_match} \
         run1_us={:?} run2_us={:?}",
        report.run1_micros, report.run2_micros,
    );
}
