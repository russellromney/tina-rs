use std::time::Duration;

use mini_saas_api::{SoakConfig, run_soak};
use tina_proof_harness::{PerfReport, assert_no_leaked_capacity_at_shutdown};

#[test]
fn perf_report_reports_whole_service_load() {
    let soak = run_soak(SoakConfig {
        workers: 3,
        op_count: 60,
        connect_timeout: Duration::from_secs(2),
    })
    .expect("run mini_saas_api local perf");

    assert_no_leaked_capacity_at_shutdown(&soak.load);
    assert!(
        soak.shutdown_clean,
        "local perf run must shut down cleanly: {}",
        soak.summary_line(),
    );
    assert!(
        soak.load.ops_ok > 0,
        "local perf run must do useful work: {}",
        soak.summary_line(),
    );

    let perf = PerfReport::from_load("mini_saas_api", "whole_service_http_db_pool", soak.load);
    println!("{}", perf.summary_line());
    println!("{}", perf.json_line());

    let summary = perf.summary_line();
    assert!(summary.contains("perf label=mini_saas_api"), "{summary}");
    assert!(summary.contains("comparison_baseline=none"), "{summary}");
    assert!(summary.contains("p50_us="), "{summary}");
    assert!(summary.contains("leak_clean=true"), "{summary}");

    let json = perf.json_line();
    assert!(
        json.contains("\"schema\":\"tina.perf_report.v2\""),
        "{json}"
    );
    assert!(json.contains("\"label\":\"mini_saas_api\""), "{json}");
    assert!(json.contains("\"comparison_baseline\":\"none\""), "{json}");
}
