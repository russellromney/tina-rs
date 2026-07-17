//! Public runner proof for the periodic-batcher specimen.
//!
//! Characterization pins the flush accounting: two size-triggered
//! flushes (items 1–5 and 6–10) and one timer-triggered flush (the
//! trailing pair). Public smoke exercises the documented Tina path.

use specimen_periodic_batcher::{Report, expected_report, tina_impl};

fn assert_batched(report: Report) {
    assert_eq!(report, expected_report());
}

/// Pins batch flush accounting before/after host-result migration.
#[test]
fn public_characterization() {
    assert_batched(tina_impl::run().expect("tina side ran"));
}

/// Documented public runner path: `tina_impl::run()`.
#[test]
fn public_smoke() {
    assert_batched(tina_impl::run().expect("tina side ran"));
}
