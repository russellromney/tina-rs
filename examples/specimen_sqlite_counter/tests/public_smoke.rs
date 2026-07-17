//! Public certification targets for the SQLite counter specimen.
//!
//! - `public_characterization` pins the pre-migration protocol facts
//!   (final value, update/query counts, clean exit, temp-DB isolation)
//!   that must survive the terminal-report migration.
//! - `public_smoke` drives the same public runners documented in the
//!   README (`tina_impl::run` / `tokio_impl::run`) and asserts the full
//!   observed report, including metrics returned through the terminal
//!   path.

use specimen_sqlite_counter::{
    INCREMENTS, Report, expected_report, tina_demo, tina_impl, tokio_impl,
};

/// Characterization of SQLite query/update behavior and metrics.
///
/// Written against the public report contract: after N increments and
/// one finalize SELECT, both sides report the same value and the same
/// application-level query/update metrics. Independent runs use
/// isolated temporary databases.
#[test]
fn public_characterization() {
    let expected = expected_report();
    assert_eq!(expected.final_value, u64::from(INCREMENTS));
    assert_eq!(expected.updates_ok, u64::from(INCREMENTS));
    assert_eq!(expected.queries_ok, 1);
    assert_eq!(expected.rows_changed, u64::from(INCREMENTS));
    assert!(expected.exit_clean);

    let tokio_report = tokio_impl::run().expect("tokio characterization");
    let tina_report = tina_impl::run().expect("tina characterization");
    assert_eq!(tokio_report, expected);
    assert_eq!(tina_report, expected);

    // Temp DB isolation: a second Tina run starts from zero again.
    let again = tina_impl::run().expect("second tina run");
    assert_eq!(again, expected);
    assert_eq!(again.final_value, u64::from(INCREMENTS));
}

/// Public smoke path: README runners produce the expected terminal
/// report, and point-in-time inspection uses the existing typed query
/// request rather than a result sidecar.
#[test]
fn public_smoke() {
    assert_eq!(tokio_impl::run().expect("tokio public runner"), expected_report());
    assert_eq!(tina_impl::run().expect("tina public runner"), expected_report());

    // Point-in-time path: host query_blocking through the helper documented
    // for mid-run inspection.
    tina_demo::demo_point_in_time_query().expect("point-in-time typed query");

    // Report fields are Copy and publicly comparable.
    let report: Report = expected_report();
    assert!(report.exit_clean);
    assert_eq!(report.updates_ok + report.queries_ok, u64::from(INCREMENTS) + 1);
}
