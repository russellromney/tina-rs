//! Smoke tests.
//!
//! Each side dynamically spawns N workers, joins their partial sums,
//! and produces the same total.

use specimen_dynamic_worker_pool::{WORKER_COUNT, expected_report, tina_impl, tokio_impl};
use tina_runtime::assert_service_owned_bound;

#[test]
fn tokio_smoke() {
    assert_eq!(
        tokio_impl::run().expect("tokio side ran"),
        expected_report()
    );
}

#[test]
fn tina_smoke() {
    let report = tina_impl::run().expect("tina side ran");
    assert_eq!(report, expected_report());
    assert_service_owned_bound(
        "specimen_dynamic_worker_pool.workers",
        Some(WORKER_COUNT as usize),
        Some(report.results_collected as usize),
    )
    .expect("worker fanout stayed under service-owned cap");
}
