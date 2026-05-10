use specimen_postgres_counter::{Report, database_url, tina_impl, tokio_impl};

fn main() -> anyhow::Result<()> {
    let mode = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "both".to_string());

    let url = match database_url() {
        Some(u) => u,
        None => {
            println!(
                "specimen_postgres_counter: skipped (DATABASE_URL not set; \
                 see README for the expected env var)"
            );
            return Ok(());
        }
    };

    match mode.as_str() {
        "tokio" => print_side("tokio", tokio_impl::run(&url)?),
        "tina" => print_side("tina", tina_impl::run(&url)?),
        "both" => {
            print_side("tokio", tokio_impl::run(&url)?);
            print_side("tina", tina_impl::run(&url)?);
        }
        other => {
            anyhow::bail!("unknown mode {other:?}; expected tokio | tina | both");
        }
    }
    Ok(())
}

fn print_side(side: &str, report: Report) {
    println!(
        "comparison=specimen_postgres_counter side={} final_value={} exit_clean={}",
        side, report.final_value, report.exit_clean,
    );
}
