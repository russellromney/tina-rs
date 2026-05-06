use std::env;
use std::process::ExitCode;

mod comparison;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    let mode = args.get(1).map(String::as_str).unwrap_or("compare");

    match mode {
        "tokio" => {
            let report = comparison::tokio_impl::run();
            comparison::print_tokio_report(&report);
            ExitCode::SUCCESS
        }
        "tina" => {
            let report = comparison::tina_impl::run();
            comparison::print_tina_report(&report);
            ExitCode::SUCCESS
        }
        "compare" => {
            let tina_report = comparison::tina_impl::run();
            comparison::print_tina_report(&tina_report);
            let tokio_report = comparison::tokio_impl::run();
            comparison::print_tokio_report(&tokio_report);
            comparison::assert_replay_distinction(&tina_report, &tokio_report);
            println!(
                "\nTina under tina-sim replays byte-identical traces; Tokio's wall-clock timings drifted."
            );
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("usage: eiffel-replay-dst [tokio|tina|compare]");
            eprintln!("unknown mode: {other}");
            ExitCode::FAILURE
        }
    }
}
