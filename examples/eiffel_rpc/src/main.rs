use eiffel_rpc::{Report, RunConfig, tina_impl, tokio_impl};

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let mode = args.next().unwrap_or_else(|| "both".to_string());
    let burst = args
        .next()
        .map(|v| v.parse::<usize>().expect("burst must be usize"))
        .unwrap_or(RunConfig::default().burst);
    let config = RunConfig { burst };

    match mode.as_str() {
        "tokio" => print_side("tokio", config, tokio_impl::run(config)?),
        "tina" => print_side("tina", config, tina_impl::run(config)?),
        "both" => {
            print_side("tokio", config, tokio_impl::run(config)?);
            print_side("tina", config, tina_impl::run(config)?);
        }
        other => {
            anyhow::bail!(
                "unknown mode {other:?}; expected tokio, tina, or both. usage: eiffel-rpc [tokio|tina|both] [burst]"
            );
        }
    }
    Ok(())
}

fn print_side(side: &str, config: RunConfig, report: Report) {
    println!(
        "comparison=eiffel_rpc side={} burst={} ok={} full={} other={}",
        side, config.burst, report.ok, report.full, report.other,
    );
}
