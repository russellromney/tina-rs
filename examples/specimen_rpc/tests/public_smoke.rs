//! Public runner proof for the RPC overload specimen.
//!
//! Characterization pins the Tina wire facts the crate's own smoke test
//! pins exactly: with `Connection::tiny_pressure()` (one in-flight slot),
//! the first request gets a `Reply` and every later request in the burst
//! comes back as a wire `Error(Full)` — `ok == 1`, `full == burst - 1` —
//! with the listener closing clean and the connection ending on
//! `CloseReason::PeerClosed`.

use specimen_rpc::{ListenerTerminal, Report, RunConfig, tina_impl};
use tina_rpc::CloseReason;

fn assert_tina_report(report: &Report, burst: usize) {
    assert_eq!(
        report.total(),
        burst,
        "every request must be accounted for: {report:?}",
    );
    assert_eq!(report.ok, 1, "one in-flight slot, one reply: {report:?}");
    assert_eq!(
        report.full,
        burst - 1,
        "over-cap requests shed as wire Error(Full): {report:?}",
    );
    assert_eq!(report.other, 0, "nothing unexpected: {report:?}");
    assert_eq!(report.wire_errors.full, report.full);
    assert_eq!(report.client_terminal, None);
    assert!(report.decode_errors.is_empty());
    assert!(report.unexpected_frames.is_empty());
    assert_eq!(
        report.listener_terminal,
        Some(ListenerTerminal::ClosedClean)
    );
    assert_eq!(report.connection_terminal, Some(CloseReason::PeerClosed));
}

/// Documented public runner path: `tina_impl::run(RunConfig::default())`
/// (the `tina` binary mode).
#[test]
fn public_smoke() {
    let config = RunConfig::default();
    let report = tina_impl::run(config).expect("tina side ran");
    assert_tina_report(&report, config.burst);
}

/// Pins the documented default burst (4) and the exact overload split
/// `ok=1 full=3` with the typed terminal facts above.
#[test]
fn public_characterization() {
    assert_eq!(RunConfig::default().burst, 4);
    let config = RunConfig::default();
    let report = tina_impl::run(config).expect("tina side ran");
    assert_tina_report(&report, config.burst);
}
