//! Smoke tests: each side connects to the responder, fires three
//! requests on one connection, and observes responses out of order.
//! `id=3` has the shortest server delay, so it should arrive first
//! — that's the property that proves real multiplexing happened.

use specimen_mux_client::{REQUEST_IDS, tina_impl, tokio_impl};

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
        "id=3 (shortest server delay) should arrive first; otherwise multiplexing didn't actually happen: {arrival_order:?}",
    );
}

#[test]
fn tokio_smoke() {
    let report = tokio_impl::run().expect("tokio side ran");
    assert_multiplexed(&report.arrival_order);
}

#[test]
fn tina_smoke() {
    let report = tina_impl::run().expect("tina side ran");
    assert_multiplexed(&report.arrival_order);
}
