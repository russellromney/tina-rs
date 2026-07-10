fn main() -> anyhow::Result<()> {
    let report = system_copied_service_path::run(system_copied_service_path::RunConfig::default())?;
    println!("{report:#?}");
    Ok(())
}
