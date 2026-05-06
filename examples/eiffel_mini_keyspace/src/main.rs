use std::process::Command;

mod comparison;

fn main() {
    let mode = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "compare".to_string());

    match mode.as_str() {
        "compare" => run_process_comparison(),
        "tokio" => print_report("tokio", comparison::run_tokio_side()),
        "tina" => print_report("tina", comparison::run_tina_side()),
        other => {
            panic!(
                "unknown mode {other:?}; expected compare, tokio, or tina. usage: eiffel-mini-keyspace [compare|tokio|tina]"
            );
        }
    }
}

fn run_process_comparison() {
    let exe = std::env::current_exe().expect("current executable path");
    for side in ["tokio", "tina"] {
        let output = Command::new(&exe)
            .arg(side)
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

fn print_report(side: &str, report: comparison::SideReport) {
    report.assert_expected();
    println!(
        "comparison=eiffel_mini_keyspace pid={} side={} ok={} values={} misses={} deleted={} response={}",
        std::process::id(),
        side,
        report.ok,
        report.values,
        report.misses,
        report.deleted,
        String::from_utf8_lossy(&report.response)
            .trim_end()
            .replace('\n', "|"),
    );
}
