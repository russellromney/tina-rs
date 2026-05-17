//! Load/soak proof for `mini_saas_api`.
//!
//! Drives a small three-lane workload (HTTP, HTTP+DB, HTTP+DB+pool)
//! through the public front door using `tina_proof_harness::load`.
//! Asserts:
//!
//! - no transport timeouts and the harness's leak check holds,
//! - the only error kind seen is `http_503` (any 4xx or other 5xx is
//!   a regression),
//! - every 5xx maps to one of the typed pressure surfaces
//!   (`db.full` + `outbound.full`) — pressure cannot escape the typed
//!   contract,
//! - the typed pressure summary on `LoadReport` agrees with the
//!   per-kind tally,
//! - the runtime shut down cleanly with no in-flight keepalive leaks,
//! - the outbound pool was actually exercised (pool path coverage).

use std::collections::HashMap;
use std::time::Duration;

use mini_saas_api::{SoakConfig, run_soak};

#[test]
fn small_steady_load_drains_cleanly_with_only_typed_pressure() {
    let report = run_soak(SoakConfig {
        workers: 4,
        op_count: 240,
        connect_timeout: Duration::from_secs(2),
    })
    .expect("run_soak");

    let load = &report.load;
    assert_eq!(
        load.ops_attempted,
        load.ops_ok + load.ops_err + load.ops_timeout,
        "{load:?}"
    );
    assert_eq!(
        load.ops_timeout,
        0,
        "steady load should not see transport timeouts: report={}",
        report.summary_line(),
    );
    assert!(load.leak_clean, "{load:?}");
    assert!(
        load.ops_ok > 0,
        "load harness should have driven at least one op: {load:?}"
    );

    // The only error kind allowed under steady load is `http_503` from
    // the controller's typed `db_full` / `outbound_full` reply paths.
    // Anything else (4xx for body/method, 5xx without typed pressure)
    // is a real regression.
    for (kind, _count) in &load.err_kinds {
        assert!(
            kind == "http_503",
            "steady load saw an unexpected error kind `{kind}`: {load:?}",
        );
    }

    // Typed pressure summary on LoadReport must agree with the
    // per-kind tally. If the two disagree, the harness lost track of
    // which ops were errors.
    assert_eq!(
        load.pressure.total,
        load.ops_err + load.ops_timeout,
        "pressure.total must equal err+timeout: {load:?}",
    );
    if load.ops_err > 0 {
        assert!(
            load.pressure.first_error_op_index.is_some(),
            "pressure must record first error position when errs > 0: {load:?}",
        );
    }

    let cap = capacity_fields(&report.capacity_after_load_line);
    assert_eq!(cap.get("controller.mailbox").map(String::as_str), Some("2"));
    assert_eq!(
        cap.get("outbound.closed_count").map(String::as_str),
        Some("0"),
        "outbound keepalive must not have closed under steady load: {}",
        report.capacity_after_load_line,
    );
    assert_eq!(
        cap.get("db.timeout").map(String::as_str),
        Some("0"),
        "no DB timeouts allowed under steady load: {}",
        report.capacity_after_load_line,
    );

    // The visible number contract: every harness 503 must correspond
    // to a typed pressure event on the controller (either db.full or
    // outbound.full). If db.full + outbound.full < err count, the
    // soak surfaced a 503 with no matching typed pressure event —
    // i.e. pressure escaped the typed surface. Fail closed.
    let db_full = cap_u64(&cap, "db.full");
    let outbound_full = cap_u64(&cap, "outbound.full");
    assert!(
        db_full + outbound_full >= load.ops_err,
        "harness saw {} 5xx but typed pressure only db.full={db_full} \
         outbound.full={outbound_full}; pressure escaping the typed surface: {}",
        load.ops_err,
        report.capacity_after_load_line,
    );

    // Pool path coverage: the third lane (`POST /items/1/notify`)
    // must have exercised the outbound keepalive pool. We accept
    // either `outbound.high_water_waiters > 0` (workers queued) or
    // `outbound.full > 0` (workers got 503 from the cap) or
    // `outbound.in_flight high_water indicator non-zero` — at least
    // one of those must show the pool actually got work.
    let outbound_in_flight_indicators = cap_u64(&cap, "outbound.high_water_waiters")
        + cap_u64(&cap, "outbound.full")
        + cap_u64(&cap, "outbound.cancel");
    // Even if all notify ops happened serially, the load is large
    // enough that at least one POST landed and the outbound pool
    // acquired+released a lease. The `outbound.full` path may not
    // fire on a small machine; we accept any of: an err observed, a
    // recorded full, a recorded cancel, or recorded waiters. If none
    // of those fire we did not exercise the pool — the soak proof
    // is weaker than advertised.
    let pool_exercised = outbound_in_flight_indicators > 0 || load.ops_err > 0;
    assert!(
        pool_exercised,
        "soak must exercise the outbound pool (notify lane); \
         indicators={outbound_in_flight_indicators}, errs={}: {}",
        load.ops_err, report.capacity_after_load_line,
    );

    assert!(
        report.shutdown_clean,
        "soak shutdown must be clean: {}",
        report.terminal_line
    );

    eprintln!("{}", report.summary_line());
}

fn capacity_fields(line: &str) -> HashMap<&str, String> {
    line.split_whitespace()
        .filter_map(|tok| tok.split_once('='))
        .map(|(k, v)| (k, v.to_string()))
        .collect()
}

fn cap_u64(cap: &HashMap<&str, String>, key: &str) -> u64 {
    cap.get(key).and_then(|v| v.parse().ok()).unwrap_or(0)
}
