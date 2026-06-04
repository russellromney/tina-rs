//! Opt-in long soak for `mini_saas_api`.
//!
//! This test is ignored by default. Run through:
//!
//! ```text
//! TINA_LONG_SOAK_SECONDS=600 make proof-long-soak
//! ```

use std::time::{Duration, Instant};

use mini_saas_api::{SoakConfig, run_soak};
use tina_proof_harness::assert_no_leaked_capacity_at_shutdown;

#[test]
#[ignore = "opt-in long soak; use make proof-long-soak"]
fn opt_in_long_soak_stays_bounded() {
    let seconds = std::env::var("TINA_LONG_SOAK_SECONDS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(600);
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut rounds = 0u64;
    let mut attempted = 0u64;
    let mut ok = 0u64;
    let mut err = 0u64;
    let mut timeout = 0u64;
    let mut max_round_p50_us = 0u64;
    let mut max_round_p99_us = 0u64;
    let mut last_capacity = String::new();
    let mut last_terminal = String::new();

    while Instant::now() < deadline || rounds == 0 {
        let report = run_soak(SoakConfig {
            workers: 4,
            op_count: 240,
            connect_timeout: Duration::from_secs(2),
        })
        .expect("run long soak round");
        assert_no_leaked_capacity_at_shutdown(&report.load);
        assert!(
            report.shutdown_clean,
            "long soak round must shut down cleanly: {}",
            report.summary_line(),
        );
        attempted += report.load.ops_attempted;
        ok += report.load.ops_ok;
        err += report.load.ops_err;
        timeout += report.load.ops_timeout;
        max_round_p50_us = max_round_p50_us.max(report.load.latency_p50_us);
        max_round_p99_us = max_round_p99_us.max(report.load.latency_p99_us);
        last_capacity = report.capacity_after_load_line;
        last_terminal = report.terminal_line;
        rounds += 1;
    }
    let final_current_zero = capacity_final_current_zero(&last_capacity);

    println!(
        "long-soak label=mini_saas_api seconds={seconds} rounds={rounds} \
         attempted={attempted} ok={ok} err={err} timeout={timeout} \
         max_round_p50_us={max_round_p50_us} max_round_p99_us={max_round_p99_us} \
         rss_delta_kb=unknown final_current_zero={final_current_zero} \
         capacity={{{last_capacity}}} terminal={{{last_terminal}}}"
    );

    assert!(attempted > 0, "long soak must drive useful work");
    assert_eq!(timeout, 0, "long soak should not see transport timeouts");
    assert!(
        final_current_zero,
        "long soak final capacity currents must drain: {last_capacity}"
    );
}

fn capacity_final_current_zero(line: &str) -> bool {
    for key in [
        "http.request_body_current",
        "http.response_body_current",
        "db.waiters",
        "db.in_flight",
        "outbound.waiters",
        "outbound.in_flight",
    ] {
        if field_u64(line, key) != Some(0) {
            return false;
        }
    }
    true
}

fn field_u64(line: &str, key: &str) -> Option<u64> {
    line.split_whitespace()
        .filter_map(|field| field.split_once('='))
        .find_map(|(field_key, value)| (field_key == key).then(|| value.parse().ok()).flatten())
}
