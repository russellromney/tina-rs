//! Public runner proof for the real-I/O chat specimen.
//!
//! Characterization pins fanout accounting and the typed connection
//! terminal path through the listener. Public smoke exercises the
//! documented Tina runner.

use specimen_real_io_chat::{Report, RunConfig, tina_impl};

fn assert_chat_report(config: RunConfig, report: Report) {
    assert_eq!(
        report.total(),
        config.burst,
        "every fanout attempt must be accounted for: {report:?}",
    );
    assert_eq!(report.delivered, report.accepted);
    assert_eq!(report.buffered, 0, "Tina must not buffer over-cap fanout");
    assert!(
        report.accepted + report.full + report.closed == config.burst,
        "accepted+full+closed must cover burst: {report:?}",
    );
}

/// Pins wire accounting and clean connection terminal observation.
#[test]
fn public_characterization() {
    let config = RunConfig::default();
    let report = tina_impl::run(config).expect("tina side ran");
    assert_chat_report(config, report);
    // Default: capacity-1 slow consumer, burst equals target cap.
    assert_eq!(report.accepted, 1);
    assert_eq!(report.full, config.burst - 1);
    assert_eq!(report.closed, 0);

    let capped = RunConfig {
        burst: 16,
        max_broadcast_targets: 4,
        slow_consumer_capacity: 1,
    };
    let capped_report = tina_impl::run(capped).expect("capped tina side ran");
    assert_chat_report(capped, capped_report);
    assert_eq!(capped_report.accepted, 1);
    assert_eq!(capped_report.full, 15);
}

/// Documented public runner path: `tina_impl::run(RunConfig)`.
#[test]
fn public_smoke() {
    let config = RunConfig::default();
    let report = tina_impl::run(config).expect("tina side ran");
    assert_chat_report(config, report);
}
