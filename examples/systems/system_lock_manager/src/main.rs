fn main() -> anyhow::Result<()> {
    let report = system_lock_manager::run(system_lock_manager::RunConfig::default())?;
    println!("{report:#?}");
    Ok(())
}
