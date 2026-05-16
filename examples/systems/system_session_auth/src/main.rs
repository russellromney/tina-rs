fn main() -> anyhow::Result<()> {
    let report = system_session_auth::run(system_session_auth::RunConfig::default())?;
    println!("{report:#?}");
    Ok(())
}
