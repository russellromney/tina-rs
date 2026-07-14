//! Smoke tests: each side runs end-to-end and accounts for every
//! request. Exact wire-shape invariants (in-flight cap, frame
//! decoding, etc.) live in `tina-rpc`'s own tests.

use specimen_rpc::{ListenerTerminal, MAX_BURST, RunConfig, tina_impl, tokio_impl};
use tina_rpc::CloseReason;

#[test]
fn tokio_smoke() {
    let config = RunConfig { burst: 4 };
    let report = tokio_impl::run(config).expect("tokio side ran");
    assert_eq!(
        report.total(),
        config.burst,
        "every request must be accounted for: {report:?}",
    );
    assert!(report.decode_errors.is_empty());
    assert!(report.unexpected_frames.is_empty());
    assert_eq!(report.other, 0);
}

#[test]
fn tina_smoke() {
    let config = RunConfig { burst: 4 };
    let report = tina_impl::run(config).expect("tina side ran");
    assert_eq!(
        report.total(),
        config.burst,
        "every request must be accounted for: {report:?}",
    );
    assert_eq!(report.ok, 1);
    assert_eq!(report.full, config.burst - 1);
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

#[test]
fn invalid_bursts_fail_before_runtime_startup() {
    for burst in [0, MAX_BURST + 1] {
        let config = RunConfig { burst };
        assert!(
            tina_impl::run(config).is_err(),
            "Tina accepted burst {burst}"
        );
        assert!(
            tokio_impl::run(config).is_err(),
            "Tokio accepted burst {burst}"
        );
    }
}
