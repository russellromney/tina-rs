fn main() -> anyhow::Result<()> {
    let report = system_live_replay_bugbox::run()?;
    println!("{}", report.summary_line);
    for d in &report.discovered {
        println!("{d}");
    }
    Ok(())
}
