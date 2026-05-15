fn main() -> anyhow::Result<()> {
    let report = system_job_queue::run(system_job_queue::RunConfig::default())?;
    println!("{report:#?}");
    Ok(())
}
