use specimen_ws_room::{Report, tina_impl, tokio_impl};

fn main() {
    let mode = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "both".to_string());

    match mode.as_str() {
        "both" | "compare" => {
            print_report("tokio", tokio_impl::run());
            print_report("tina", tina_impl::run());
        }
        "tokio" => print_report("tokio", tokio_impl::run()),
        "tina" => print_report("tina", tina_impl::run()),
        other => panic!("unknown mode {other:?}; usage: specimen-ws-room [both|compare|tokio|tina]"),
    }
}

fn print_report(side: &str, report: Report) {
    report.assert_expected();
    println!(
        "side={} alpha={:?} bravo={:?}",
        side, report.alpha_inbox, report.bravo_inbox
    );
}
