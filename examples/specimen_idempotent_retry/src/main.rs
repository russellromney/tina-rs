use specimen_idempotent_retry::{RunConfig, run};

fn main() -> anyhow::Result<()> {
    let report = run(RunConfig::default())?;
    println!(
        "idempotent_retry key={:#x} outcome={:?} attempts={} retries={} downstream_charges={}",
        report.idempotency_key,
        report.outcome,
        report.attempts,
        report.retries,
        report.downstream_charges,
    );
    Ok(())
}
