//! Public runner proof for the axum counter specimen.
//!
//! Characterization pins the scripted three-step HTTP exchange over the
//! Tina bridge: POST, POST, GET return statuses 200/200/200 with bodies
//! 1, 2, 2. Public smoke exercises the documented Tina runner path.

use specimen_axum_counter::tina_impl;

/// Pins the exact wire exchange through the Tina-bridged axum service.
#[test]
fn public_characterization() {
    let report = tina_impl::run().expect("tina side ran");
    // The documented script: increment, increment, read.
    assert_eq!(
        report.statuses,
        vec![200, 200, 200],
        "all three counter requests should return 200"
    );
    assert_eq!(
        report.bodies,
        vec!["1".to_string(), "2".to_string(), "2".to_string()],
        "increment, increment, get should produce 1, 2, 2"
    );
}

/// Documented public runner path: `tina_impl::run()`
/// (`cargo run --manifest-path examples/specimen_axum_counter/Cargo.toml -- tina`).
#[test]
fn public_smoke() {
    let report = tina_impl::run().expect("tina side ran");
    report.assert_expected();
}
