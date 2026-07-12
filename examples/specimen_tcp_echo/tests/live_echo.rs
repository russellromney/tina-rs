//! Live loopback tests: real `ThreadedRuntime`, real sockets.
//!
//! The bound address arrives through `observe_next_bound`, and the
//! listener stop through `observe_isolate_complete` (both inside
//! `echo_round_trip`) — no sleep-as-synchronization, no shared-slot
//! polling.

use specimen_tcp_echo::{echo_round_trip, run_load_shed};

#[test]
fn echoes_identical_bytes_over_loopback() {
    let payload = b"the quick brown llama echoes over 127.0.0.1";
    let echoed = echo_round_trip(payload).expect("echo round trip ran");
    assert_eq!(echoed, payload, "echoed bytes must equal sent bytes");
}

#[test]
fn empty_read_after_half_close_still_returns_prior_bytes() {
    // A single byte still round-trips; proves the read/echo/read loop
    // terminates on the EOF that the client's half-close produces.
    let echoed = echo_round_trip(b"x").expect("echo round trip ran");
    assert_eq!(echoed, b"x");
}

#[test]
fn burst_past_capacity_sheds_typed_full() {
    // Bounded mailbox: a producer that outruns the worker gets a typed
    // Full. capacity 4, burst 32 -> the surplus is shed, never queued.
    let report = run_load_shed(32, 4).expect("load shed ran");
    assert_eq!(
        report.total(),
        32,
        "every burst record must be accounted for: {report:?}",
    );
    assert!(
        report.full > 0,
        "burst of 32 at capacity 4 must observe at least one typed Full, got {report:?}",
    );
}
