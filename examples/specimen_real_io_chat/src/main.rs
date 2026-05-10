use specimen_real_io_chat::{Report, RunConfig, tina_impl, tokio_impl};
use tina_runtime::{PressureReport, format_pressure_line};

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let mode = args.next().unwrap_or_else(|| "both".to_string());
    let burst = args
        .next()
        .map(|v| v.parse::<usize>().expect("burst must be usize"))
        .unwrap_or(RunConfig::default().burst);
    let config = RunConfig {
        burst,
        ..Default::default()
    };

    match mode.as_str() {
        "tokio" => print_side("tokio", config, tokio_impl::run(config)?),
        "tina" => print_side("tina", config, tina_impl::run(config)?),
        "both" | "compare" => {
            print_side("tokio", config, tokio_impl::run(config)?);
            print_side("tina", config, tina_impl::run(config)?);
        }
        other => {
            anyhow::bail!(
                "unknown mode {other:?}; expected tokio, tina, both, or compare. \
                 usage: specimen-real-io-chat [tokio|tina|both|compare] [burst]"
            );
        }
    }
    Ok(())
}

fn print_side(side: &str, config: RunConfig, report: Report) {
    println!(
        "comparison=specimen_real_io_chat side={} burst={} accepted={} full={} closed={} delivered={} buffered={}",
        side,
        config.burst,
        report.accepted,
        report.full,
        report.closed,
        report.delivered,
        report.buffered,
    );
    // Phase 059 Rock 9: print the canonical key=value line so a
    // pressure runner can intercept and surface it.
    println!(
        "{}",
        format_pressure_line(&PressureReport {
            side,
            accepted: report.accepted as u64,
            full: report.full as u64,
            closed: report.closed as u64,
            timeouts: 0,
            other: 0,
            rss_peak_kb: None,
            exit: "clean",
        })
    );
}
