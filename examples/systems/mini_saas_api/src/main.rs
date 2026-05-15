use mini_saas_api::{RunMode, run};

fn main() -> anyhow::Result<()> {
    let mode = match std::env::args().nth(1).as_deref() {
        None | Some("smoke") => RunMode::Smoke,
        Some("pressure") => RunMode::Pressure,
        Some(other) => anyhow::bail!(
            "unknown mode {other:?}; expected `smoke` or `pressure`. usage: mini-saas-api [smoke|pressure]"
        ),
    };

    let report = run(mode)?;
    println!("{}", report.summary_line());
    println!("{}", report.capacity_before_shutdown_line.trim_end());
    println!("{}", report.capacity_during_shutdown_line.trim_end());
    println!("{}", report.terminal_line);
    println!("live_replay_fact {}", report.live_replay_fact);
    Ok(())
}
