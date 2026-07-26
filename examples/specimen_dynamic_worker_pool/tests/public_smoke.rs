//! Public runner proof for the dynamic-worker-pool specimen.
//!
//! Characterization pins the fixed workload: 4 workers joining disjoint
//! slices of `1..=16` into a total of 136, with every spawn/call
//! terminal bucket accounted for. Public smoke exercises the documented
//! Tina runner path.

use specimen_dynamic_worker_pool::{
    WORK_VALUES, WORKER_COUNT, assert_tina_report_accounted, expected_report, tina_impl,
};
use tina_runtime::assert_service_owned_bound;

/// Pins the fanout count and the joined total exactly.
#[test]
fn public_characterization() {
    assert_eq!(WORKER_COUNT, 4);
    assert_eq!(WORK_VALUES.len(), 16);
    assert_eq!(WORK_VALUES.iter().sum::<u64>(), 136);

    let report = tina_impl::run().expect("tina side ran");
    assert_tina_report_accounted(&report);
    assert_eq!(report, expected_report());
    assert_eq!(report.results_collected, WORKER_COUNT);
    assert_eq!(report.total_sum, 136);
    assert!(report.exit_clean);
}

/// Documented public runner path: `tina_impl::run()`
/// (`cargo run --manifest-path examples/specimen_dynamic_worker_pool/Cargo.toml -- both`).
#[test]
fn public_smoke() {
    let report = tina_impl::run().expect("tina side ran");
    assert_tina_report_accounted(&report);
    assert_eq!(report, expected_report());
    assert_service_owned_bound(
        "specimen_dynamic_worker_pool.workers",
        Some(WORKER_COUNT as usize),
        Some(report.results_collected as usize),
    )
    .expect("worker fanout stayed under service-owned cap");
}
