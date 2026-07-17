fn main() -> Result<(), system_session_auth::RunError> {
    let report = system_session_auth::run(system_session_auth::RunConfig::default())?;
    println!("{report:#?}");
    Ok(())
}
