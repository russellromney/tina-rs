//! Public runner proof for the multiplexed-client specimen.
//!
//! Characterization pins the out-of-order arrival: `id=3` has the
//! shortest server delay, so it must arrive first — that's the proof
//! real multiplexing happened. Public smoke exercises the documented
//! Tina path against the loopback responder.

use specimen_mux_client::{REQUEST_IDS, tina_impl};

fn assert_multiplexed(arrival_order: &[u32]) {
    assert_eq!(
        arrival_order.len(),
        REQUEST_IDS.len(),
        "every request must produce a response: {arrival_order:?}",
    );
    let mut sorted = arrival_order.to_vec();
    sorted.sort();
    assert_eq!(
        sorted,
        REQUEST_IDS.to_vec(),
        "every requested id must appear once: {arrival_order:?}",
    );
    assert_eq!(
        arrival_order.first(),
        Some(&3),
        "id=3 (shortest server delay) should arrive first: {arrival_order:?}",
    );
}

/// Pins multiplexed arrival order before/after host-result migration.
#[test]
fn public_characterization() {
    let report = tina_impl::run().expect("tina side ran");
    assert_multiplexed(&report.arrival_order);
}

/// Documented public runner path: `tina_impl::run()`.
#[test]
fn public_smoke() {
    let report = tina_impl::run().expect("tina side ran");
    assert_multiplexed(&report.arrival_order);
}
