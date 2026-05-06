use std::process::Command;

use eiffel_rpc::comparison;

fn main() {
    let mut args = std::env::args().skip(1);
    let mode = args.next().unwrap_or_else(|| "compare".to_string());
    let burst = args
        .next()
        .map(|value| {
            value
                .parse::<usize>()
                .expect("burst argument must be usize")
        })
        .unwrap_or(comparison::ComparisonConfig::default().burst);

    match mode.as_str() {
        "compare" => run_process_comparison(burst),
        "tokio" => print_side("tokio", burst, comparison::run_tokio_side(burst)),
        "tina" => print_side("tina", burst, comparison::run_tina_side(burst)),
        other => {
            panic!(
                "unknown mode {other:?}; expected compare, tokio, or tina. usage: eiffel-rpc [compare|tokio|tina] [burst]"
            );
        }
    }
}

fn run_process_comparison(burst: usize) {
    let exe = std::env::current_exe().expect("current executable path");
    for side in ["tokio", "tina"] {
        let output = Command::new(&exe)
            .arg(side)
            .arg(burst.to_string())
            .output()
            .unwrap_or_else(|error| panic!("spawn {side} comparison process: {error}"));
        if !output.status.success() {
            panic!(
                "{side} comparison process failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
                output.status.code(),
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
        }
        print!("{}", String::from_utf8_lossy(&output.stdout));
    }
}

fn print_side(side: &str, burst: usize, report: comparison::SideReport) {
    println!(
        "comparison=eiffel_rpc side={} burst={} ok={} full={} other={}",
        side, burst, report.ok, report.full, report.other,
    );
}
