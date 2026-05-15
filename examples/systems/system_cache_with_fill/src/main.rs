fn main() -> anyhow::Result<()> {
    let report = system_cache_with_fill::run(system_cache_with_fill::RunConfig::default())?;
    println!("{report:#?}");
    Ok(())
}
