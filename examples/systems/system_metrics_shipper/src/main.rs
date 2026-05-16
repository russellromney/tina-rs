fn main() -> anyhow::Result<()> {
    let report = system_metrics_shipper::run(system_metrics_shipper::RunConfig::default())?;
    println!("{report:#?}");
    Ok(())
}
