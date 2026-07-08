//! The graceful drain must let already-admitted in-flight requests finish
//! (real response, pool lease released) before force-cancelling stragglers.
//! Reproduces the reviewer's scenario: a slow notify held mid-outbound while
//! the shutdown choreography runs.

use std::time::Duration;

use mini_saas_api::prove_graceful_drain_completes_in_flight;

#[test]
fn graceful_drain_lets_in_flight_notify_complete() {
    // Deadline comfortably longer than the ~250ms slow notify: the request
    // must complete on its own with its real answer, and every resource —
    // including the outbound keepalive lease — must close cleanly.
    let proof = prove_graceful_drain_completes_in_flight(Duration::from_secs(5))
        .expect("graceful drain proof ran");

    assert_eq!(
        proof.in_flight_status, 200,
        "in-flight notify must complete with 200, not be dropped: status={} body={:?}",
        proof.in_flight_status, proof.in_flight_body,
    );
    assert!(
        proof.in_flight_notified,
        "in-flight notify must return its real `notified` answer: body={:?}",
        proof.in_flight_body,
    );
    assert!(
        proof.shutdown_clean,
        "drain must be clean (lease released, no pool timeout): terminal={:?}",
        proof.terminal_line,
    );
    assert!(
        proof.terminal_line.contains("outbound.drain=Drained"),
        "outbound pool must drain, not time out on a stranded lease: {:?}",
        proof.terminal_line,
    );
    assert!(
        proof.scopes_drain_line.contains("unreleased=0"),
        "sweep after a clean drain finds nothing stranded: {:?}",
        proof.scopes_drain_line,
    );
}

#[test]
fn straggler_past_deadline_forces_unclean_drain() {
    // Deadline shorter than the in-flight work: the notify is still parked at
    // the deadline, gets force-cancelled, and its lease is stranded — so the
    // pool drain times out and the drain reports unclean. This is the case
    // `serve` turns into a non-zero exit.
    let proof = prove_graceful_drain_completes_in_flight(Duration::from_millis(1))
        .expect("graceful drain proof ran");

    assert!(
        !proof.in_flight_notified,
        "a force-cancelled straggler must NOT report `notified`: status={} body={:?}",
        proof.in_flight_status, proof.in_flight_body,
    );
    assert!(
        !proof.shutdown_clean,
        "a stranded lease past the deadline must make the drain unclean: terminal={:?}",
        proof.terminal_line,
    );
    assert!(
        proof.terminal_line.contains("outbound.drain=TimedOut"),
        "the stranded lease must surface as an outbound pool drain timeout: {:?}",
        proof.terminal_line,
    );
}
