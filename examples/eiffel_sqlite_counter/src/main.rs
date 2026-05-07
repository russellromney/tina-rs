use eiffel_sqlite_counter::{Report, tina_demo, tina_impl, tokio_impl};

fn main() -> anyhow::Result<()> {
    let mode = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "both".to_string());

    match mode.as_str() {
        "tokio" => print_side("tokio", tokio_impl::run()?),
        "tina" => print_side("tina", tina_impl::run()?),
        "both" => {
            print_side("tokio", tokio_impl::run()?);
            print_side("tina", tina_impl::run()?);
        }
        "demo" => {
            tina_demo::demo_constraint()?;
            tina_demo::demo_timeout()?;
            tina_demo::demo_closed()?;
            tina_demo::demo_invalid()?;
        }
        "demo-constraint" => tina_demo::demo_constraint()?,
        "demo-timeout" => tina_demo::demo_timeout()?,
        "demo-closed" => tina_demo::demo_closed()?,
        "demo-invalid" => tina_demo::demo_invalid()?,
        other => {
            anyhow::bail!(
                "unknown mode {other:?}; expected tokio | tina | both | demo \
                 | demo-constraint | demo-timeout | demo-closed | demo-invalid"
            );
        }
    }
    Ok(())
}

fn print_side(side: &str, report: Report) {
    println!(
        "comparison=eiffel_sqlite_counter side={} final_value={} exit_clean={}",
        side, report.final_value, report.exit_clean,
    );
}
