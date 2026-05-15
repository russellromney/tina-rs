fn main() -> anyhow::Result<()> {
    let report = system_bounded_object_lane::run(system_bounded_object_lane::RunConfig::default())?;
    println!("{report:#?}");
    Ok(())
}
