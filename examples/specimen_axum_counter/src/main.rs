use specimen_axum_counter::{Report, tina_impl, tokio_impl};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mode = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "both".to_string());

    match mode.as_str() {
        "both" | "compare" => {
            print_report("tokio", tokio_impl::run());
            print_report("tina", tina_impl::run()?);
        }
        "tokio" => print_report("tokio", tokio_impl::run()),
        "tina" => print_report("tina", tina_impl::run()?),
        other => {
            panic!("unknown mode {other:?}; usage: specimen-axum-counter [both|compare|tokio|tina]")
        }
    }
    Ok(())
}

fn print_report(side: &str, report: Report) {
    report.assert_expected();
    println!(
        "side={} statuses={:?} bodies={:?}",
        side, report.statuses, report.bodies,
    );
}
