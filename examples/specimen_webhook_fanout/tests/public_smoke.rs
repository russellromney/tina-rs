//! Public runner proof for the webhook-fanout specimen.
//!
//! Characterization pins the endpoint bucketing: two healthy
//! deliveries, one `503`, one timeout, nothing unclassified. Public
//! smoke exercises the documented Tina path through
//! `tina-reqwest-bridge` and reuses the crate's invariant
//! assertions.

use specimen_webhook_fanout::{Report, WebhookTerminal, assert_report_invariants, tina_impl};
use tina_reqwest_bridge::ReqwestTransientReason;

fn assert_fanout(report: &Report) {
    assert_report_invariants("public", report);
    assert_eq!(report.tina_terminals.len(), 2, "{report:?}");
    assert!(report.tina_terminals.iter().any(|terminal| matches!(
        terminal,
        WebhookTerminal::Transient(ReqwestTransientReason::UpstreamServer { status })
            if status.as_u16() == 503
    )));
    // Which timeout reason fires (bridge cap vs worker timer) is
    // timing-dependent; the pin is that exactly one of them appears.
    assert!(report.tina_terminals.iter().any(|terminal| matches!(
        terminal,
        WebhookTerminal::Transient(
            ReqwestTransientReason::BridgeTimeout | ReqwestTransientReason::WorkerTimeout
        )
    )));
}

/// Pins endpoint bucketing before/after host-result migration.
#[test]
fn public_characterization() {
    assert_fanout(&tina_impl::run().expect("tina side ran"));
}

/// Documented public runner path: `tina_impl::run()`.
#[test]
fn public_smoke() {
    assert_fanout(&tina_impl::run().expect("tina side ran"));
}
