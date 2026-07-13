fn main() -> anyhow::Result<()> {
    let config = system_bounded_object_lane::RunConfig::from_env()?;
    let report = system_bounded_object_lane::run(config)?;
    println!("{report:#?}");
    Ok(())
}
