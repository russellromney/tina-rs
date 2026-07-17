//! Public runner proof for the mini-keyspace specimen.
//!
//! Characterization pins the fixed script counts (`SET / GET / GET /
//! DEL / GET / QUIT` → `ok=1, values=1, misses=2, deleted=1`). Public
//! smoke exercises the documented Tina path over real loopback TCP.

use specimen_mini_keyspace::{Report, tina_impl};

const EXPECTED: Report = Report {
    ok: 1,
    values: 1,
    misses: 2,
    deleted: 1,
};

fn assert_script_counts(report: Report) {
    assert_eq!(report, EXPECTED);
}

/// Pins script counts before/after host-result migration.
#[test]
fn public_characterization() {
    assert_script_counts(tina_impl::run().expect("tina side ran"));
}

/// Documented public runner path: `tina_impl::run()`.
#[test]
fn public_smoke() {
    assert_script_counts(tina_impl::run().expect("tina side ran"));
}
