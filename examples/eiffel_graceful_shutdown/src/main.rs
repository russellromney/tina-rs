use std::env;
use std::process::{Command, ExitCode, Stdio};

mod comparison;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    let mode = args.get(1).map(String::as_str).unwrap_or("compare");

    match mode {
        "tokio" => {
            let report = comparison::tokio_impl::run();
            comparison::print_machine_report("tokio", &report);
            comparison::print_human_report("tokio", &report);
            ExitCode::SUCCESS
        }
        "tina" => {
            let report = comparison::tina_impl::run();
            comparison::print_machine_report("tina", &report);
            comparison::print_human_report("tina", &report);
            ExitCode::SUCCESS
        }
        "compare" => {
            // Subprocesses keep each side's signal-handler installation
            // isolated. Running them in the same process risks chained
            // SIGINT handlers from one side firing during the other.
            let exe = env::current_exe().expect("current exe");
            let tokio_report = run_subprocess(&exe, "tokio");
            comparison::print_human_report("tokio", &tokio_report);
            let tina_report = run_subprocess(&exe, "tina");
            comparison::print_human_report("tina", &tina_report);
            comparison::assert_equivalent(&tokio_report, &tina_report);
            println!(
                "\nboth services drained their pending work after a real SIGINT and exited cleanly."
            );
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("usage: eiffel-graceful-shutdown [tokio|tina|compare]");
            eprintln!("unknown mode: {other}");
            ExitCode::FAILURE
        }
    }
}

fn run_subprocess(exe: &std::path::Path, side: &str) -> comparison::SideReport {
    let output = Command::new(exe)
        .arg(side)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .output()
        .unwrap_or_else(|err| panic!("spawn {side} subprocess: {err:?}"));
    if !output.status.success() {
        panic!("{side} subprocess failed: {}", output.status);
    }
    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    comparison::parse_machine_report(&stdout)
        .unwrap_or_else(|| panic!("{side} subprocess produced no machine report:\n{stdout}"))
}
