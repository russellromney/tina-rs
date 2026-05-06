use std::env;
use std::process::ExitCode;

mod comparison;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    let mode = args.get(1).map(String::as_str).unwrap_or("compare");

    match mode {
        "tokio" => {
            let report = comparison::tokio_impl::run();
            comparison::print_report("tokio", &report);
            ExitCode::SUCCESS
        }
        "tina" => {
            let report = comparison::tina_impl::run();
            comparison::print_report("tina", &report);
            ExitCode::SUCCESS
        }
        "compare" => {
            let tokio_report = comparison::tokio_impl::run();
            comparison::print_report("tokio", &tokio_report);
            let tina_report = comparison::tina_impl::run();
            comparison::print_report("tina", &tina_report);
            comparison::assert_equivalent(&tokio_report, &tina_report);
            println!(
                "\nboth client implementations fetched the same payload count from a real loopback server."
            );
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("usage: eiffel-outbound-fetch [tokio|tina|compare]");
            eprintln!("unknown mode: {other}");
            ExitCode::FAILURE
        }
    }
}
