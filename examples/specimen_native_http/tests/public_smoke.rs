//! Public runner proof for the native-HTTP counter specimen.
//!
//! Characterization pins the scripted counts (`GET → POST × 3 → GET
//! → GET /missing`). Public smoke exercises the documented Tina path
//! against the scripted std::net client.

use specimen_native_http::{Report, tina_impl};

const EXPECTED: Report = Report {
    successful_get: 2,
    successful_post: 3,
    final_counter_value: 3,
    got_404_for_missing: true,
    exit_clean: true,
};

fn assert_scripted(report: Report) {
    assert_eq!(report, EXPECTED);
}

/// Pins scripted HTTP counts before/after host-result migration.
#[test]
fn public_characterization() {
    assert_scripted(tina_impl::run().expect("tina side ran"));
}

/// Documented public runner path: `tina_impl::run()`.
#[test]
fn public_smoke() {
    assert_scripted(tina_impl::run().expect("tina side ran"));
}
